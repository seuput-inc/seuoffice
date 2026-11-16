use std::sync::{Arc, mpsc};

use serde_json::Value;
use shell_contract::ipc::{
    NativeExistingDocumentBinding, NativeInvocationTarget, NativeRequestOwner,
};
use shell_contract::runtime::{CancelSubscription, CancelToken, RendererGeneration};
use tauri::Webview;

use crate::native::defer::is_ui_thread;
use crate::native::{BrowserState, Done, DocumentIdentity, Fence, PageError, PdfSpec, WebviewExt};

/// The document a print is bound to: the browser and its committed document at capture.
#[derive(Clone)]
pub(crate) struct Target {
    pub frame: BrowserState,
    pub document: DocumentIdentity,
    pub generation: RendererGeneration,
}

/// Cancels its token when dropped, so a print's actor lives exactly as long as its call.
pub(crate) struct ActorLifetime(pub CancelToken);

impl Drop for ActorLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone)]
pub(crate) enum Authority {
    Pair {
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
    },
    Native {
        invocation: NativeInvocationTarget,
        owner: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        stop: CancelToken,
    },
    ExistingDocument {
        binding: NativeExistingDocumentBinding,
        participants: Vec<NativeRequestOwner>,
        additional: Vec<CancelToken>,
        stop: CancelToken,
    },
    OwnedMany {
        origin: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        additional: Vec<CancelToken>,
        stop: CancelToken,
    },
    NativeManyExtra {
        invocation: NativeInvocationTarget,
        owner: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        additional: Vec<CancelToken>,
        stop: CancelToken,
    },
}

#[derive(Clone)]
pub(crate) struct Owner {
    pub authority: Authority,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub actor: CancelToken,
    pub target: Target,
}

/// A page operation's error as the print caller reads it.
pub(crate) fn owned_error(error: PageError) -> String {
    match error {
        PageError::Cancelled => "Print request expired.".to_owned(),
        PageError::Stale => "Print request is no longer available.".to_owned(),
        other => super::page_error_text(other),
    }
}

impl Owner {
    /// Run `effect` only while every owner is live and the print target is still the
    /// browser's committed document.
    pub fn effect<R>(&self, effect: impl FnOnce() -> R) -> Result<R, String> {
        use crate::ipc::native_owner as owners;
        let action = || {
            if self.target.frame.document().as_ref() != Some(&self.target.document) {
                return None;
            }
            Some(effect())
        };
        let (cancel, pending, generation) = (&self.cancel, &self.pending, &self.target.generation);
        let result = match &self.authority {
            Authority::Pair { origin, destination } => owners::with_print_pair_effect(
                origin, destination, generation, &self.actor, cancel, pending, action,
            ),
            Authority::Native { invocation, owner, participants, stop } => {
                owners::with_native_invocation_print_many_effect(
                    invocation,
                    owner,
                    participants,
                    cancel,
                    pending,
                    &self.actor,
                    stop,
                    generation,
                    action,
                )
            }
            Authority::ExistingDocument { binding, participants, additional, stop } => {
                owners::with_existing_document_print_effect(
                    binding,
                    participants,
                    cancel,
                    pending,
                    stop,
                    generation,
                    &additional.iter().collect::<Vec<_>>(),
                    action,
                )
            }
            Authority::OwnedMany { origin, participants, additional, stop } => {
                owners::with_owned_print_many_extra_effect(
                    origin,
                    participants,
                    cancel,
                    pending,
                    stop,
                    generation,
                    &additional.iter().collect::<Vec<_>>(),
                    action,
                )
            }
            Authority::NativeManyExtra { invocation, owner, participants, additional, stop } => {
                owners::with_native_invocation_print_many_extra_effect(
                    invocation,
                    owner,
                    participants,
                    cancel,
                    pending,
                    stop,
                    generation,
                    &additional.iter().collect::<Vec<_>>(),
                    action,
                )
            }
        };
        result.flatten().ok_or_else(|| "Print request is no longer available.".into())
    }

    pub fn losses(&self) -> Result<Vec<CancelToken>, String> {
        self.authority.losses(&self.cancel, &self.pending, &self.actor, &self.target.generation)
    }

    /// The one authority a page operation takes: its effect starts only while `effect`
    /// would be admitted.
    pub fn fence(&self) -> Fence {
        let owner = self.clone();
        Arc::new(move |start: &mut dyn FnMut()| owner.effect(|| start()))
    }

    /// Start one page operation under this authority and wait for its answer on the calling
    /// thread. Losing any owner wakes the wait; the answer is admitted once more before it
    /// is returned.
    pub fn page<T: Send + 'static>(
        &self, start: impl FnOnce(Fence, Done<T>),
    ) -> Result<T, PageError> {
        if is_ui_thread() {
            return Err(PageError::Platform(
                "A print page operation was started on the UI thread.".into(),
            ));
        }
        self.effect(|| ()).map_err(|_| PageError::Stale)?;
        let (send, receive) = mpsc::sync_channel(1);
        let _losses = subscribe_losses(self.losses().map_err(PageError::Platform)?, &send);
        start(
            self.fence(),
            Box::new(move |result| {
                let _ = send.try_send(result);
            }),
        );
        let answer = receive.recv_timeout(super::PAGE_TIMEOUT).map_err(|_| PageError::Timeout)?;
        let value = answer?;
        self.effect(|| value).map_err(|_| PageError::Stale)
    }

    pub fn print_pdf(&self, webview: &Webview, spec: PdfSpec) -> Result<Vec<u8>, String> {
        let page = webview.page()?;
        self.page(|fence, done| page.print_pdf(spec, Some(fence), done)).map_err(owned_error)
    }

    pub fn evaluate(
        &self, webview: &Webview, js: &str, await_promise: bool,
    ) -> Result<Value, PageError> {
        let page = webview.page().map_err(PageError::Platform)?;
        self.page(|fence, done| page.evaluate(js, await_promise, Some(fence), done))
    }
}

impl Authority {
    pub(crate) fn losses(
        &self, cancel: &CancelToken, pending: &CancelToken, actor: &CancelToken,
        generation: &RendererGeneration,
    ) -> Result<Vec<CancelToken>, String> {
        let mut losses =
            vec![cancel.clone(), pending.clone(), actor.clone(), generation.loss_token()];
        match self {
            Authority::Pair { origin, destination } => {
                losses.push(origin.loss_token().ok_or("Print origin lifetime is unavailable.")?);
                losses.push(
                    destination.loss_token().ok_or("Print destination lifetime is unavailable.")?,
                );
            }
            Authority::Native { invocation, owner, participants, stop } => {
                losses.push(stop.clone());
                losses.push(
                    owner.loss_token().ok_or("Print destination lifetime is unavailable.")?,
                );
                for participant in participants {
                    losses.push(
                        participant
                            .loss_token()
                            .ok_or("Print participant lifetime is unavailable.")?,
                    );
                }
                push_invocation(&mut losses, invocation);
            }
            Authority::ExistingDocument { binding, participants, additional, stop } => {
                losses.push(stop.clone());
                losses.extend(additional.iter().cloned());
                push_leases(&mut losses, binding.owner());
                losses.push(
                    binding.owner().loss_token().ok_or("Print document lifetime is unavailable.")?,
                );
                for participant in participants {
                    losses.push(
                        participant
                            .loss_token()
                            .ok_or("Print participant lifetime is unavailable.")?,
                    );
                    push_leases(&mut losses, participant);
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
                    push_leases(&mut losses, owner);
                }
                if let Authority::NativeManyExtra { invocation, .. } = self {
                    push_invocation(&mut losses, invocation);
                }
            }
        }
        Ok(losses)
    }
}

fn push_invocation(losses: &mut Vec<CancelToken>, invocation: &NativeInvocationTarget) {
    losses.push(invocation.created().loss_token());
    losses.push(invocation.operation().cancel().clone());
    losses.push(invocation.operation().shutdown().clone());
    losses.push(invocation.operation().loss_token());
}

fn push_leases(losses: &mut Vec<CancelToken>, owner: &NativeRequestOwner) {
    if let Some(document) = owner.context.document_lease() {
        losses.push(document.loss_token());
    }
    if let Some(document) = owner.pdf_document() {
        losses.push(document.loss_token());
    }
    if let Some(revision) = owner.pdf_revision() {
        losses.push(revision.loss_token());
    }
}

/// Every loss answers the waiting call once with `Cancelled`; a token that is already lost
/// answers during the subscription.
pub(crate) fn subscribe_losses<T: Send + 'static>(
    losses: Vec<CancelToken>, answer: &mpsc::SyncSender<Result<T, PageError>>,
) -> Vec<CancelSubscription> {
    losses
        .into_iter()
        .map(|loss| {
            let answer = answer.clone();
            loss.on_cancel(move || {
                let _ = answer.try_send(Err(PageError::Cancelled));
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
