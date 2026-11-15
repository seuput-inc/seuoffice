use std::rc::Rc;
use shell_contract::{ViewId, hosts::CommittedPdfReload, ipc::{NativeRequestOwner, PdfDocumentLease}, runtime::RendererGeneration};
use super::{Editor, editors::PdfEditors};

pub(super) struct PendingSavedReload {
    input: shell_contract::ipc::NativeSavedInput,
    path: String,
    path_witness: shell_contract::ipc::NativeSavedPath,
    renderer: RendererGeneration,
    token: String,
    handoff: Option<std::sync::Arc<shell_contract::hosts::NativeSaveReloadHandoff>>,
    completion: Option<AdmittedPdfReloadCompletion>,
    done: Option<shell_contract::hosts::NativeSaveReloadDone>,
    waiter: Option<(shell_contract::ipc::CallContext, shell_contract::ipc::Responder)>,
    losses: Vec<shell_contract::runtime::CancelSubscription>,
    consumed: bool,
    settling: bool,
    swapped: bool,
}

pub(super) struct SavedDocument {
    input: shell_contract::ipc::NativeSavedInput,
    path: String,
    receipt: std::sync::Arc<shell_contract::hosts::NativeSaveReloadReceipt>,
}

struct SavedActivationRollback {
    editor: Rc<Editor>,
    view: ViewId,
    renderer: RendererGeneration,
    armed: bool,
}

impl Drop for SavedActivationRollback {
    fn drop(&mut self) {
        if self.armed { cancel_saved(&self.editor, self.view, &self.renderer, "PDF reload activation failed"); }
    }
}

pub(super) fn has_saved_reload(editor: &Editor, view: ViewId) -> bool {
    editor.st.borrow().views.get(&view).is_some_and(|slot| slot.saved_reload.is_some())
}

pub(super) fn cancel_saved(editor: &Editor, view: ViewId, renderer: &RendererGeneration, message: &str) {
    let removed = {
        let mut state = editor.st.borrow_mut();
        let Some(slot) = state.views.get_mut(&view) else { return; };
        if !slot.saved_reload.as_ref().is_some_and(|pending| pending.renderer.same_generation(renderer)) { return; }
        let removed = slot.saved_reload.take();
        if let Some(pending) = &removed {
            if pending.swapped { slot.saved_failed = Some(pending.path.clone()); }
        }
        removed
    };
    if let Some(pending) = removed { fail_pending(pending, message); }
}

pub(super) fn fail_pending(mut pending: PendingSavedReload, message: &str) {
    let losses = std::mem::take(&mut pending.losses);
    let waiter = pending.waiter.take();
    let done = pending.done.take();
    drop(losses);
    drop(pending);
    if let Some((_, reply)) = waiter { reply.reply(Err(crate::kit::error(message))); }
    if let Some(done) = done { done(Err(message.to_owned())); }
}

pub(super) fn consume_saved(editor: &Editor, cx: &shell_contract::ipc::CallContext, reply: shell_contract::ipc::Responder) {
    use crate::kit::IntoIpc;
    let Some(context) = cx.native.clone() else { return reply.reply(Err(crate::kit::error("PDF reload sender is not authenticated"))); };
    let actual = NativeRequestOwner { view: cx.sender.view, context };
    let handoff = {
        let mut state = editor.st.borrow_mut();
        let Some(pending) = state.views.get_mut(&cx.sender.view).and_then(|slot| slot.saved_reload.as_mut()) else {
            drop(state);
            return reply.reply(Err(crate::kit::error("PDF reload input is unavailable")));
        };
        if !actual.context.renderer().is_some_and(|renderer| renderer.same_generation(&pending.renderer)) {
            drop(state);
            return reply.reply(Err(crate::kit::error("PDF reload sender is not the reserved renderer")));
        }
        match pending.handoff.clone() {
            Some(handoff) => handoff,
            None if pending.waiter.is_none() => { pending.waiter = Some((cx.clone(), reply)); return; }
            None => { drop(state); return reply.reply(Err(crate::kit::error("PDF reload input is already waiting"))); }
        }
    };
    let Some(effects) = editor.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>() else {
        return reply.reply(Err(crate::kit::error("PDF reload admission is unavailable")));
    };
    let mut answer = None;
    let admitted = effects.with_native_save_reload_parser_effect(&handoff, &actual, &cx.cancel, &mut |_| {
        let mut state = editor.st.borrow_mut();
        let pending = state.views.get_mut(&cx.sender.view).and_then(|slot| slot.saved_reload.as_mut())
            .ok_or("The PDF saved reload barrier is unavailable")?;
        if pending.consumed || !pending.handoff.as_ref().is_some_and(|current| std::sync::Arc::ptr_eq(current, &handoff)) {
            return Err("The PDF saved reload was already consumed".into());
        }
        pending.consumed = true;
        answer = Some(crate::kit::obj([("path", pending.path.clone().into_ipc()), ("token", pending.token.clone().into_ipc())]));
        Ok(())
    });
    match admitted {
        Ok(()) => reply.reply(answer.ok_or_else(|| crate::kit::error("The PDF reload input was not admitted"))),
        Err(error) => reply.reply(Err(crate::kit::error(error))),
    }
}

pub(super) fn settle_saved(
    editor: &Editor, cx: &shell_contract::ipc::CallContext, token: &str, result: Result<(), String>,
) -> Result<(), String> {
    let context = cx.native.clone().ok_or("PDF reload sender is not authenticated")?;
    let actual = NativeRequestOwner { view: cx.sender.view, context };
    let handoff = editor.st.borrow().views.get(&cx.sender.view)
        .and_then(|slot| slot.saved_reload.as_ref()).and_then(|pending| pending.handoff.clone())
        .ok_or("The PDF saved reload barrier is unavailable")?;
    let effects = editor.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
        .ok_or("PDF reload admission is unavailable")?;
    let mut claimed = None;
    effects.with_native_save_reload_parser_effect(&handoff, &actual, &cx.cancel, &mut |_| {
        let mut state = editor.st.borrow_mut();
        let pending = state.views.get_mut(&cx.sender.view).and_then(|slot| slot.saved_reload.as_mut())
            .ok_or("The PDF saved reload barrier is unavailable")?;
        if !pending.consumed || pending.settling || pending.token != token
            || !pending.handoff.as_ref().is_some_and(|current| std::sync::Arc::ptr_eq(current, &handoff)) {
            return Err("The PDF reload acknowledgement does not match its pending input".into());
        }
        if pending.completion.is_none() || pending.done.is_none() { return Err("The PDF reload completion is unavailable".into()); }
        let completion = pending.completion.take().expect("checked completion");
        let done = pending.done.take().expect("checked callback");
        pending.settling = true;
        claimed = Some((completion, done));
        Ok(())
    })?;
    let (completion, done) = claimed.ok_or("The PDF reload acknowledgement was not admitted")?;
    let completed = match result {
        Ok(()) => completion.finish().and_then(|completed| handoff.complete(completed)).map(std::sync::Arc::new),
        Err(error) => Err(error),
    };
    let mut retired_pending = None;
    let mut previous_document = None;
    let completed = completed.and_then(|receipt| {
        let path = handoff.path().path().to_str().ok_or("The PDF saved path is not representable")?.to_owned();
        let mut document = Some(SavedDocument { input: handoff.input().clone(), path, receipt: receipt.clone() });
        effects.with_native_save_reload_many_effect(&receipt, &actual, &[], &cx.cancel, &cx.cancel, &mut || {
            let mut state = editor.st.borrow_mut();
            let slot = state.views.get_mut(&cx.sender.view).ok_or("The PDF saved reload view is unavailable")?;
            let pending = slot.saved_reload.as_ref().ok_or("The PDF saved reload barrier is unavailable")?;
            if !pending.settling || pending.token != token { return Err("The PDF saved reload barrier changed".into()); }
            let replacement = document.take().ok_or("The PDF saved document was already installed")?;
            retired_pending = slot.saved_reload.take();
            previous_document = slot.saved_document.replace(replacement);
            Ok(())
        })?;
        Ok(receipt)
    });
    drop(retired_pending);
    drop(previous_document);
    if completed.is_err() {
        let removed = {
            let mut state = editor.st.borrow_mut();
            let slot = state.views.get_mut(&cx.sender.view);
            slot.and_then(|slot| {
                let pending = slot.saved_reload.take();
                if let Some(pending) = &pending {
                    if pending.swapped { slot.saved_failed = Some(pending.path.clone()); }
                }
                pending
            })
        };
        drop(removed);
    }
    done(completed);
    Ok(())
}

#[derive(Clone)]
pub(super) struct SavedReadScope {
    input: shell_contract::ipc::NativeSavedInput,
    authority: SavedReadAuthority,
    actual: NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    effects: shell_contract::hosts::OwnedEffectsHost,
}

#[derive(Clone)]
enum SavedReadAuthority {
    Pending(std::sync::Arc<shell_contract::hosts::NativeSaveReloadHandoff>),
    Ready(std::sync::Arc<shell_contract::hosts::NativeSaveReloadReceipt>),
}

impl SavedReadScope {
    pub(super) fn bytes(&self) -> &[u8] { self.input.bytes() }
    pub(super) fn admit(&self, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        match &self.authority {
            SavedReadAuthority::Ready(receipt) => self.effects.with_native_save_reload_reader_effect(receipt, &self.actual, &self.cancel, effect),
            SavedReadAuthority::Pending(handoff) => self.effects.with_native_save_reload_parser_effect(handoff, &self.actual, &self.cancel, &mut |_| effect()),
        }
    }
}

pub(super) fn saved_read_scope(editor: &Editor, cx: &shell_contract::ipc::CallContext, path: &str) -> Result<Option<SavedReadScope>, String> {
    let (input, authority) = {
        let state = editor.st.borrow();
        let Some(slot) = state.views.get(&cx.sender.view) else { return Ok(None); };
        if slot.saved_failed.is_some() { return Err("The PDF saved document did not finish loading".into()); }
        if let Some(pending) = &slot.saved_reload {
            if !pending.consumed || pending.path != path || pending.settling {
                return Err("The PDF saved input does not match its admitted document".into());
            }
            (pending.input.clone(), SavedReadAuthority::Pending(pending.handoff.clone().ok_or("The PDF saved input is not ready")?))
        } else if let Some(document) = &slot.saved_document {
            if document.path != path { return Err("The PDF saved input does not match its admitted document".into()); }
            (document.input.clone(), SavedReadAuthority::Ready(document.receipt.clone()))
        } else { return Ok(None); }
    };
    let actual = NativeRequestOwner { view: cx.sender.view, context: cx.native.clone().ok_or("PDF reload sender is not authenticated")? };
    let effects = editor.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned()
        .ok_or("PDF reload admission is unavailable")?;
    let scope = SavedReadScope { input, authority, actual, cancel: cx.cancel.clone(), effects };
    scope.admit(&mut || Ok(()))?;
    Ok(Some(scope))
}

/// Staged on the core actor before native reload admission. No renderer-supplied
/// path or id participates in this file-slot identity.
pub struct PreparedPdfReload {
    editor: Rc<Editor>,
    predecessor: NativeRequestOwner,
    view: ViewId,
    path: String,
    old_slot: PdfDocumentLease,
    old_revision: PdfDocumentLease,
    next_renderer: RendererGeneration,
    next_slot: PdfDocumentLease,
    next_revision: PdfDocumentLease,
    saved_path: Option<String>,
    saved_grant: Option<std::collections::HashSet<String>>,
    committed: bool,
}

pub struct AdmittedPdfReloadCompletion {
    editor: Rc<Editor>,
    view: ViewId,
    owner: NativeRequestOwner,
    lineage: shell_contract::hosts::AcceptedPdfReload,
    old_slot: PdfDocumentLease,
    old_revision: PdfDocumentLease,
    retired_in_gate: bool,
    old_revision_subscription: Option<shell_contract::runtime::CancelSubscription>,
    old_saved_document: Option<SavedDocument>,
    armed: bool,
}

impl AdmittedPdfReloadCompletion {

    /// Install the real renderer acknowledgement continuation after the actor swap.
    pub fn await_saved_ack(
        self, handoff: std::sync::Arc<shell_contract::hosts::NativeSaveReloadHandoff>,
        done: shell_contract::hosts::NativeSaveReloadDone,
    ) -> Result<(), String> {
        let editor = self.editor.clone();
        let view = self.view;
        let renderer = self.lineage.reserved_renderer().clone();
        let mut rollback = SavedActivationRollback { editor: editor.clone(), view, renderer: renderer.clone(), armed: true };
        let mut controls = vec![handoff.cancel().clone(), handoff.pending().clone(), handoff.browser_loss().clone(), handoff.reservation_loss().clone()];
        controls.extend(handoff.additional().iter().cloned());
        let mut owners = handoff.completion_participants(&self.owner)?;
        owners.push(self.owner.clone());
        match handoff.publication() {
            shell_contract::ipc::SavedPublication::Native(value) => {
                let operation = value.selection().binding().operation();
                controls.extend([operation.cancel().clone(), operation.shutdown().clone(), operation.authority_generation().loss_token(), value.cancel().clone(), value.pending().clone()]);
                controls.extend(value.additional().iter().cloned());
                owners.extend(value.participants().iter().map(|owner| if handoff.retires(owner) { self.owner.clone() } else { owner.clone() }));
            }
            shell_contract::ipc::SavedPublication::Renderer(value) => {
                controls.extend([value.cancel().clone(), value.pending().clone()]);
                controls.extend(value.additional().iter().cloned());
                owners.push(if handoff.retires(value.destination()) { self.owner.clone() } else { value.destination().clone() });
                owners.extend(value.participants().iter().map(|owner| if handoff.retires(owner) { self.owner.clone() } else { owner.clone() }));
            }
        }
        match handoff.path() {
            shell_contract::ipc::NativeSavedPath::Selected(_) => {}
            shell_contract::ipc::NativeSavedPath::DriveMoved(receipt) | shell_contract::ipc::NativeSavedPath::DriveFallback(receipt) => {
                let snapshot = receipt.preflight_reopen()?;
                controls.extend(snapshot.controls().iter().cloned());
                owners.extend(snapshot.participants().iter().map(|owner| if handoff.retires(owner) { self.owner.clone() } else { owner.clone() }));
            }
        }
        for owner in owners {
            controls.push(owner.loss_token().ok_or("A PDF reload owner lifetime is unavailable")?);
            controls.push(owner.context.renderer().ok_or("A PDF reload renderer is unavailable")?.loss_token());
        }
        let mut losses = Vec::with_capacity(controls.len());
        for control in controls {
            let weak = Rc::downgrade(&editor);
            let renderer = renderer.clone();
            let back = crate::kit::back_on_core(&editor.services, move |()| {
                if let Some(editor) = weak.upgrade() { cancel_saved(&editor, view, &renderer, "PDF reload authority was lost"); }
            });
            losses.push(control.on_cancel(move || back(())));
        }
        let waiter = {
            let mut state = editor.st.borrow_mut();
            let pending = state.views.get_mut(&view).and_then(|slot| slot.saved_reload.as_mut())
                .ok_or("The PDF saved reload barrier is unavailable")?;
            if pending.handoff.is_some() || !pending.path_witness.same_witness(handoff.path())
                || !pending.input.same_input(handoff.input()) {
                return Err("The PDF saved reload barrier changed".into());
            }
            pending.handoff = Some(handoff);
            pending.completion = Some(self);
            pending.done = Some(done);
            pending.losses = losses;
            pending.waiter.take()
        };
        rollback.armed = false;
        if let Some((cx, reply)) = waiter { consume_saved(&editor, &cx, reply); }
        Ok(())
    }
    pub fn before_notify(&mut self) {
        drop(self.old_revision_subscription.take());
        drop(self.old_saved_document.take());
    }
    pub fn finish(mut self) -> Result<CommittedPdfReload, String> {
        self.armed = false;
        self.before_notify();
        self.editor.stop_view_watch(self.view);
        if !self.retired_in_gate {
            let old_slot = self.old_slot.revoke_deferred();
            let old_revision = self.old_revision.revoke_deferred();
            old_slot.cancel();
            old_revision.cancel();
        }
        CommittedPdfReload::from_completed_native_load(self.owner.clone(), self.lineage.clone())
    }
}

impl Drop for AdmittedPdfReloadCompletion {
    fn drop(&mut self) {
        if self.armed {
            cancel_saved(&self.editor, self.view, self.lineage.reserved_renderer(), "PDF reload completion was discarded");
        }
    }
}

impl PreparedPdfReload {
    pub fn predecessor_slot(&self) -> &PdfDocumentLease { &self.old_slot }
    pub fn predecessor_revision(&self) -> &PdfDocumentLease { &self.old_revision }
    pub fn successor_slot(&self) -> &PdfDocumentLease { &self.next_slot }
    pub fn successor_revision(&self) -> &PdfDocumentLease { &self.next_revision }

    /// Compare actor identity without reacquiring lifetime gates held by the
    /// enclosing multi-owner admission.
    pub fn matches_actor(&self) -> bool {
        let state = self.editor.st.borrow();
        state.views.get(&self.view).is_some_and(|slot| {
            slot.open_path.as_deref() == Some(self.path.as_str())
                && slot.document_lease.as_ref().is_some_and(|lease| lease.same_document(&self.old_slot))
                && slot.document_revision.as_ref().is_some_and(|revision| revision.same_document(&self.old_revision))
                && slot.reload_renderer.as_ref().is_some_and(|renderer| renderer.same_generation(&self.next_renderer))
        })
    }

    /// Consume only the native producer's completed, exact reserved document.
    pub fn complete(mut self, native: NativeRequestOwner, lineage: shell_contract::hosts::AcceptedPdfReload) -> Result<CommittedPdfReload, String> {
        if !native.context.renderer().is_some_and(|renderer| renderer.is_active())
            || native.loss_token().is_none_or(|loss| loss.is_cancelled()) {
            return Err("The captured PDF reload was replaced before completion.".into());
        }
        let owner = self.prepare_loaded_owner(native)?;
        self.swap(owner, lineage, None)?.finish()
    }

    pub fn prepare_loaded_owner(&self, native: NativeRequestOwner) -> Result<NativeRequestOwner, String> {
        if native.view != self.view || !native.context.renderer().is_some_and(|renderer| renderer.same_generation(&self.next_renderer)) {
            return Err("The loaded PDF renderer does not match its reservation.".into());
        }
        Ok(NativeRequestOwner { view: self.view, context: native.context.with_pdf_document(self.next_slot.clone()).with_pdf_revision(self.next_revision.clone()) })
    }

    pub fn complete_admitted(&mut self, native: NativeRequestOwner, lineage: shell_contract::hosts::AcceptedPdfReload, retirement: &mut shell_contract::ipc::AdmittedPdfReloadRetirement<'_, '_>) -> Result<AdmittedPdfReloadCompletion, String> {
        self.swap(native, lineage, Some(retirement))
    }

    fn swap(&mut self, native: NativeRequestOwner, lineage: shell_contract::hosts::AcceptedPdfReload, mut retirement: Option<&mut shell_contract::ipc::AdmittedPdfReloadRetirement<'_, '_>>) -> Result<AdmittedPdfReloadCompletion, String> {
        if native.view != self.view || !lineage.retires(&self.predecessor)
            || !native.context.renderer().is_some_and(|renderer| renderer.same_generation(&self.next_renderer))
            || !native.pdf_document().is_some_and(|slot| slot.same_document(&self.next_slot))
            || !native.pdf_revision().is_some_and(|revision| revision.same_document(&self.next_revision))
            || !self.matches_actor()
        { return Err("The captured PDF reload was replaced before completion.".into()); }
        let owner = native;
        let (old_revision_subscription, old_saved_document) = {
            let mut state = self.editor.st.borrow_mut();
            let slot = state.views.get_mut(&self.view).ok_or("The PDF view was closed during reload.")?;
            if self.saved_path.is_some() && (self.saved_grant.is_none() || slot.saved_reload.is_none()) {
                return Err("The PDF saved input barrier was replaced".into());
            }
            if let Some(retirement) = &mut retirement { retirement.retire()?; }
            if let Some(path) = self.saved_path.take() {
                self.editor.grants.0.insert(self.view, self.saved_grant.take().expect("checked grant"));
                slot.open_path = Some(path);
                slot.saved_reload.as_mut().expect("checked barrier").swapped = true;
                slot.saved_failed = None;
            }
            slot.document_lease = Some(self.next_slot.clone());
            slot.document_revision = Some(self.next_revision.clone());
            slot.reload_renderer = None;
            slot.dirty = false;
            (slot.recovery_seed.as_mut().and_then(super::recovery_open::PendingSeed::take_revision_loss), slot.saved_document.take())
        };
        self.committed = true;
        Ok(AdmittedPdfReloadCompletion { editor: self.editor.clone(), view: self.view, owner, lineage, old_slot: self.old_slot.clone(), old_revision: self.old_revision.clone(), retired_in_gate: retirement.is_some(), old_revision_subscription, old_saved_document, armed: true })
    }
}

impl Drop for PreparedPdfReload {
    fn drop(&mut self) {
        if self.committed { return; }
        let removed = {
            let mut state = self.editor.st.borrow_mut();
            if let Some(slot) = state.views.get_mut(&self.view) {
                if slot.reload_renderer.as_ref().is_some_and(|renderer| renderer.same_generation(&self.next_renderer)) {
                    slot.reload_renderer = None;
                }
                if slot.saved_reload.as_ref().is_some_and(|pending| pending.renderer.same_generation(&self.next_renderer)) {
                    slot.saved_reload.take()
                } else { None }
            } else { None }
        };
        if let Some(pending) = removed { fail_pending(pending, "PDF reload was cancelled"); }
        let slot = self.next_slot.revoke_deferred();
        let revision = self.next_revision.revoke_deferred();
        slot.cancel();
        revision.cancel();
    }
}

impl PdfEditors {
    pub fn prepare_saved_reload(
        &self, existing: &NativeRequestOwner, next_renderer: RendererGeneration,
        input: shell_contract::ipc::NativeSavedInput, path: shell_contract::ipc::NativeSavedPath,
    ) -> Result<PreparedPdfReload, String> {
        if !path.matches_publication(input.output()) {
            return Err("The PDF saved input does not match its publication".into());
        }
        let path_witness = path;
        let path = path_witness.path().to_str().ok_or("The PDF saved path is not representable")?.to_owned();
        let token = jscompat::random_uuid();
        let mut prepared = self.prepare_generated_reload(existing, next_renderer.clone())?;
        prepared.saved_path = Some(path.clone());
        prepared.saved_grant = Some(std::collections::HashSet::from([path.clone()]));
        {
            let mut state = self.0.st.borrow_mut();
            let slot = state.views.get_mut(&existing.view).ok_or("The captured PDF view is unavailable")?;
            if slot.saved_reload.is_some() || slot.recovery_preparing.is_some() || slot.recovery_seed.as_ref().is_some_and(|seed| !seed.completed()) {
                return Err("The PDF view already has a pending input".into());
            }
            slot.saved_reload = Some(PendingSavedReload {
                input, path, path_witness, renderer: next_renderer, token, handoff: None, completion: None, done: None, waiter: None,
                losses: Vec::new(), consumed: false, settling: false, swapped: false,
            });
        }
        Ok(prepared)
    }

    pub fn prepare_generated_reload(&self, existing: &NativeRequestOwner, next_renderer: RendererGeneration) -> Result<PreparedPdfReload, String> {
        if !self.0.services.core.is_current() { return Err("PDF reload requires the core thread.".into()); }
        let old_slot = existing.pdf_document().filter(|slot| slot.is_live()).ok_or("The PDF reload has no captured file slot.")?.clone();
        let old_revision = existing.pdf_revision().filter(|revision| revision.is_live()).ok_or("The PDF reload has no captured revision.")?.clone();
        if !next_renderer.is_active() || existing.context.renderer().is_none_or(|renderer| renderer.same_generation(&next_renderer)) {
            return Err("The PDF reload requires its reserved successor renderer.".into());
        }
        let mut state = self.0.st.borrow_mut();
        let slot = state.views.get_mut(&existing.view).ok_or("The captured PDF is no longer open.")?;
        if slot.reload_renderer.is_some()
            || !slot.document_lease.as_ref().is_some_and(|current| current.same_document(&old_slot))
            || !slot.document_revision.as_ref().is_some_and(|current| current.same_document(&old_revision))
        { return Err("The captured PDF file slot changed before reload.".into()); }
        let path = slot.open_path.clone().ok_or("The captured PDF has no file to reload.")?;
        slot.reload_renderer = Some(next_renderer.clone());
        Ok(PreparedPdfReload { editor: self.0.clone(), predecessor: existing.clone(), view: existing.view, path, old_slot, old_revision,
            next_renderer, next_slot: PdfDocumentLease::new(), next_revision: PdfDocumentLease::new(), saved_path: None, saved_grant: None, committed: false })
    }
}

#[cfg(test)]
mod tests;

pub use crate::kit::back_on_core;
