use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelSubscription, CancelToken};

#[derive(Clone)]
pub(crate) struct Target {
    pub frame: tauri_runtime_cef::FrameNavigationState,
    pub document: tauri_runtime_cef::NativeDocumentToken,
    pub generation: shell_contract::runtime::RendererGeneration,
}

#[derive(Clone)]
pub(super) enum Authority {
    Pair { origin: NativeRequestOwner, destination: NativeRequestOwner },
    Native { invocation: shell_contract::ipc::NativeInvocationTarget, owner: NativeRequestOwner, participants: Vec<NativeRequestOwner>, stop: CancelToken },
    ExistingDocument { binding: shell_contract::ipc::NativeExistingDocumentBinding, participants: Vec<NativeRequestOwner>, additional: Vec<CancelToken>, stop: CancelToken },
    OwnedMany { origin: NativeRequestOwner, participants: Vec<NativeRequestOwner>, additional: Vec<CancelToken>, stop: CancelToken },
    NativeManyExtra { invocation: shell_contract::ipc::NativeInvocationTarget, owner: NativeRequestOwner, participants: Vec<NativeRequestOwner>, additional: Vec<CancelToken>, stop: CancelToken },
}

#[derive(Clone)]
pub(super) struct Owner {
    pub authority: Authority,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub actor: CancelToken,
    pub target: Target,
}
impl Owner {
    pub fn effect<R>(&self, effect: impl FnOnce() -> R) -> Result<R, String> {
        let action = || {
            if self.target.frame.document().as_ref() != Some(&self.target.document) { return None; }
            Some(effect())
        };
        let result = match &self.authority {
            Authority::Pair { origin, destination } => crate::ipc::native_owner::with_print_pair_effect(
                origin, destination, &self.target.generation, &self.actor, &self.cancel, &self.pending, action),
            Authority::Native { invocation, owner, participants, stop } => crate::ipc::native_owner::with_native_invocation_print_many_effect(
                invocation, owner, participants, &self.cancel, &self.pending, &self.actor, stop, &self.target.generation, action),
            Authority::ExistingDocument { binding, participants, additional, stop } => crate::ipc::native_owner::with_existing_document_print_effect(
                binding, participants, &self.cancel, &self.pending, stop, &self.target.generation,
                &additional.iter().collect::<Vec<_>>(), action),
            Authority::OwnedMany { origin, participants, additional, stop } => crate::ipc::native_owner::with_owned_print_many_extra_effect(
                origin, participants, &self.cancel, &self.pending, stop, &self.target.generation, &additional.iter().collect::<Vec<_>>(), action),
            Authority::NativeManyExtra { invocation, owner, participants, additional, stop } => crate::ipc::native_owner::with_native_invocation_print_many_extra_effect(
                invocation, owner, participants, &self.cancel, &self.pending, stop, &self.target.generation, &additional.iter().collect::<Vec<_>>(), action),
        };
        result.flatten().ok_or_else(|| "Print request is no longer available.".into())
    }
    pub fn losses(&self) -> Result<Vec<CancelToken>, String> {
        self.authority.losses(&self.cancel, &self.pending, &self.actor, &self.target.generation)
    }
    pub fn subscribe(&self, id: i32) -> Result<Vec<CancelSubscription>, String> {
        Ok(subscribe_losses(self.losses()?, id))
    }
}

impl Authority {
    pub(super) fn losses(&self, cancel: &CancelToken, pending: &CancelToken, actor: &CancelToken,
        generation: &shell_contract::runtime::RendererGeneration) -> Result<Vec<CancelToken>, String> {
        let mut losses = vec![cancel.clone(), pending.clone(), actor.clone(), generation.loss_token()];
        match self {
            Authority::Pair { origin, destination } => {
                losses.push(origin.loss_token().ok_or("Print origin lifetime is unavailable.")?);
                losses.push(destination.loss_token().ok_or("Print destination lifetime is unavailable.")?);
            }
            Authority::Native { invocation, owner, participants, stop } => {
                losses.push(stop.clone());
                losses.push(owner.loss_token().ok_or("Print destination lifetime is unavailable.")?);
                for participant in participants {
                    losses.push(participant.loss_token().ok_or("Print participant lifetime is unavailable.")?);
                }
                losses.push(invocation.created().loss_token());
                losses.push(invocation.operation().cancel().clone());
                losses.push(invocation.operation().shutdown().clone());
                losses.push(invocation.operation().loss_token());
            }
            Authority::ExistingDocument { binding, participants, additional, stop } => {
                losses.push(stop.clone());
                losses.extend(additional.iter().cloned());
                if let Some(document) = binding.owner().context.document_lease() { losses.push(document.loss_token()); }
                if let Some(document) = binding.owner().pdf_document() { losses.push(document.loss_token()); }
                if let Some(revision) = binding.owner().pdf_revision() { losses.push(revision.loss_token()); }
                losses.push(binding.owner().loss_token().ok_or("Print document lifetime is unavailable.")?);
                for participant in participants {
                    losses.push(participant.loss_token().ok_or("Print participant lifetime is unavailable.")?);
                    if let Some(document) = participant.context.document_lease() { losses.push(document.loss_token()); }
                    if let Some(document) = participant.pdf_document() { losses.push(document.loss_token()); }
                    if let Some(revision) = participant.pdf_revision() { losses.push(revision.loss_token()); }
                }
                losses.push(binding.operation().cancel().clone());
                losses.push(binding.operation().shutdown().clone());
                losses.push(binding.operation().loss_token());
            }
            Authority::OwnedMany { origin, participants, additional, stop }
            | Authority::NativeManyExtra { owner: origin, participants, additional, stop, .. } => {
                losses.push(stop.clone());
                losses.extend(additional.iter().cloned());
                for owner in std::iter::once(origin).chain(participants.iter()) {
                    losses.push(owner.loss_token().ok_or("Print owner lifetime is unavailable.")?);
                    if let Some(document) = owner.context.document_lease() { losses.push(document.loss_token()); }
                    if let Some(document) = owner.pdf_document() { losses.push(document.loss_token()); }
                    if let Some(revision) = owner.pdf_revision() { losses.push(revision.loss_token()); }
                }
                if let Authority::NativeManyExtra { invocation, .. } = self {
                    losses.push(invocation.created().loss_token());
                    losses.push(invocation.operation().cancel().clone());
                    losses.push(invocation.operation().shutdown().clone());
                    losses.push(invocation.operation().loss_token());
                }
            }
        }
        Ok(losses)
    }
}

fn subscribe_losses(losses: Vec<CancelToken>, id: i32) -> Vec<CancelSubscription> {
    losses.into_iter().map(|loss| loss.on_cancel(move || {
        let call = super::PENDING.0.lock().ok().and_then(|mut calls| calls.remove(&id));
        if let Some(call) = call { let _ = call.sender.try_send(Err("Print request expired.".into())); }
    })).collect()
}

#[cfg(test)]
mod tests;
