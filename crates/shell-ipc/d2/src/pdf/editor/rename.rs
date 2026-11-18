use std::{path::Path, rc::Rc};
use shell_contract::{ViewId, editors::Then, hosts::OwnedEffectsHost, ipc::NativeRequestOwner, runtime::CancelToken};
use super::{Editor, recovery::RecoveryRename};
use crate::kit::{self, IntoIpc};

#[derive(Clone)]
enum RenameOrigin {
    Renderer(NativeRequestOwner),
    Native(shell_contract::ipc::NativeSaveRevisionSnapshot, std::path::PathBuf),
}

#[derive(Clone)]
pub(super) struct RenameAuthority {
    origin: RenameOrigin,
    receipt: Option<shell_contract::ipc::RecoverySeedReceipt>,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
}
impl RenameAuthority {
    pub(super) fn effect(&self, action: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        let mut used = false;
        let mut admitted = || {
            if let RenameOrigin::Native(snapshot, expected) = &self.origin {
                if snapshot.selection().path() != expected { return Err("PDF native save destination changed".into()); }
            }
            if used { return Err("PDF rename effect was invoked more than once.".into()); }
            used = true;
            action()
        };
        let additional: Vec<_> = self.additional.iter().collect();
        match (&self.receipt, &self.origin) {
            (Some(receipt), RenameOrigin::Renderer(origin)) => self.effects.with_recovery_open_effect_many_extra(receipt, origin, &self.participants, &self.cancel, &self.pending, &additional, &mut admitted)?,
            (None, RenameOrigin::Renderer(origin)) => self.effects.with_owned_effect_many_extra(origin, &self.participants, &self.cancel, &self.pending, &additional, &mut admitted)?,
            (None, RenameOrigin::Native(snapshot, _)) => self.effects.with_native_save_revision_snapshot_effect(snapshot, &self.participants, &self.cancel, &self.pending, &additional, &mut admitted)?,
            (Some(_), RenameOrigin::Native(_, _)) => return Err("PDF recovery rename requires its captured receiver".into()),
        }
        if used { Ok(()) } else { Err("PDF rename effect was not admitted.".into()) }
    }
    fn emit(&self, channel: &str, args: Vec<shell_contract::IpcValue>) -> Result<(), String> {
        match (&self.receipt, &self.origin) {
            (Some(receipt), RenameOrigin::Renderer(origin)) => self.effects.emit_recovery_open_many(receipt, origin, &self.participants, 0, &self.cancel, &self.pending, channel, args),
            (None, RenameOrigin::Renderer(origin)) => self.effects.emit_owned_many_extra(origin, &self.participants, 0, &self.cancel, &self.pending, &self.additional.iter().collect::<Vec<_>>(), channel, args),
            (None, RenameOrigin::Native(snapshot, expected)) if snapshot.selection().path() == expected => self.effects.emit_native_save_revision_snapshot(snapshot, &self.participants, 0, &self.cancel, &self.pending, &self.additional.iter().collect::<Vec<_>>(), channel, args),
            (None, RenameOrigin::Native(_, _)) => Err("PDF native save destination changed".into()),
            (Some(_), RenameOrigin::Native(_, _)) => Err("PDF recovery rename requires its captured receiver".into()),
        }
    }
    pub(super) fn admit_file(&self, action: &mut dyn FnMut()) -> Result<(), main_core::node_fs::NodeError> {
        self.effect(&mut || { action(); Ok(()) }).map_err(|message| main_core::node_fs::NodeError { code: Some("ECANCELED"), message })
    }
}

pub(super) fn run(editor: &Rc<Editor>, view: ViewId, old: &Path, new: &Path, origin: &NativeRequestOwner,
    destination: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken], receipt: Option<&shell_contract::ipc::RecoverySeedReceipt>, then: Then<()>) {
    run_impl(editor, view, old, new, RenameOrigin::Renderer(origin.clone()), destination, participants, cancel, pending, additional, receipt, Box::new(move |result| then(result.map(|_| ()))));
}

pub(super) fn run_native(editor: &Rc<Editor>, view: ViewId, old: &Path, new: &Path, authority: &shell_contract::ipc::NativeSaveRevisionAuthority, expected_path: &Path,
    destination: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken], then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>) {
    let snapshot = match authority.snapshot() { Ok(snapshot) => snapshot, Err(error) => return then(Err(kit::error(error))) };
    run_impl(editor, view, old, new, RenameOrigin::Native(snapshot, expected_path.to_path_buf()), destination, participants, cancel, pending, additional, None, then);
}

fn run_impl(editor: &Rc<Editor>, view: ViewId, old: &Path, new: &Path, origin: RenameOrigin,
    destination: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken], receipt: Option<&shell_contract::ipc::RecoverySeedReceipt>, then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>) {
    let Some(effects) = editor.services.hosts.get::<OwnedEffectsHost>().cloned() else { return then(Err(kit::error("PDF owned effects unavailable"))); };
    if destination.view != view || destination.pdf_document().is_none() || destination.pdf_revision().is_none() { return then(Err(kit::error("pdf: missing captured document lease"))); }
    let mut all = Vec::with_capacity(participants.len() + 1);
    all.push(destination.clone());
    all.extend_from_slice(participants);
    let mut authority = RenameAuthority { origin, receipt: receipt.cloned(), participants: all, cancel: cancel.clone(), pending: pending.clone(), additional: additional.to_vec(), effects };
    let work = match receipt.map(|receipt| receipt.begin_work()).transpose() {
        Ok(work) => work,
        Err(error) => return then(Err(kit::error(error))),
    };
    let (old, new) = (old.to_string_lossy().into_owned(), new.to_string_lossy().into_owned());
    let mut recovery: Option<RecoveryRename> = None;
    let mut watched = false;
    let mut alias = false;
    let changed = authority.effect(&mut || {
        let state = editor.st.borrow();
        let slot = state.views.get(&view).ok_or("pdf: document unavailable")?;
        if !destination.pdf_document().zip(slot.document_lease.as_ref()).is_some_and(|(a, b)| a.same_document(b))
            || !destination.pdf_revision().zip(slot.document_revision.as_ref()).is_some_and(|(a, b)| a.same_document(b))
        { return Err("pdf: document changed".into()); }
        alias = authority.receipt.as_ref().is_some_and(|receipt| slot.recovery_seed.as_ref()
            .and_then(super::recovery_open::PendingSeed::pending_target)
            .is_some_and(|target| receipt.matches(&target)));
        drop(state);
        if !alias {
            watched = editor.file_renamed_records(view, &old, &new);
            recovery = editor.rec.borrow_mut().prepare_renamed(view, &old, &new);
        }
        Ok(())
    });
    if let Err(error) = changed { return then(Err(kit::error(error))); }
    if alias {
        let RenameOrigin::Renderer(origin) = &authority.origin else { return then(Err(kit::error("PDF recovery rename requires its captured receiver"))); };
        let Some(receipt) = authority.receipt.as_ref() else { return then(Err(kit::error("PDF recovery receipt is unavailable"))); };
        let Some(old_revision) = destination.pdf_revision().cloned() else { return then(Err(kit::error("PDF recovery revision is unavailable"))); };
        let handoff = std::cell::RefCell::new(super::recovery_open::RevisionHandoff::prepare(receipt, &old_revision, old.clone(), new.clone()));
        let successor = handoff.borrow().successor().clone();
        let explicit: Vec<_> = std::iter::once(origin).chain(authority.participants.iter()).cloned().collect();
        let snapshot = match receipt.target().authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return then(Err(kit::error(error))),
        };
        let mut renewal = match snapshot.prepare_pdf_revision_renewal(&explicit, &old_revision, &successor) {
            Ok(renewal) => renewal,
            Err(error) => return then(Err(kit::error(error))),
        };
        let mut before_notify = || handoff.borrow_mut().before_notify();
        let mut transition = |admitted: &mut shell_contract::ipc::AdmittedPdfRevisionRetirement<'_, '_>,
                              renewal: &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal| {
            handoff.borrow_mut().transition(editor, view, || renewal.commit(admitted))?;
            watched = editor.file_renamed_records(view, &old, &new);
            recovery = editor.rec.borrow_mut().prepare_renamed(view, &old, &new);
            Ok(())
        };
        if let Err(error) = authority.effects.with_recovery_pdf_revision_transition(
            receipt, origin, &authority.participants, &authority.cancel, &authority.pending,
            &old_revision, &successor, &mut renewal, &mut before_notify, &mut transition,
        ) { return then(Err(kit::error(error))); }
        drop(transition);
        drop(before_notify);
        for (index, owner) in explicit.iter().enumerate() {
            if owner.pdf_revision().is_some_and(|revision| revision.same_document(&old_revision)) {
                let Some(owner) = renewal.successor_for(owner).cloned() else { return then(Err(kit::error("PDF recovery successor was not admitted"))); };
                if index == 0 { authority.origin = RenameOrigin::Renderer(owner); } else { authority.participants[index - 1] = owner; }
            }
        }
    }
    let store = editor.rec.borrow().store.clone();
    let worker_authority = authority.clone();
    let worker_work = work.clone();
    let completed = editor.clone();
    kit::on_worker(&editor.services, move || {
        let _work = worker_work;
        worker_authority.effect(&mut || Ok(()))?;
        let moved = match &recovery {
            Some(recovery) => recovery.run(&store, Some(&worker_authority)), None => Ok(false),
        };
        Ok((recovery, moved))
    }, move |result: Result<_, String>| {
        let _work = work;
        let (recovery, moved) = match result { Ok(result) => result, Err(error) => return then(Err(kit::error(error))) };
        if let Err(error) = &moved {
            if error.code == Some("ECANCELED") { return then(Err(kit::error(error.message.clone()))); }
            eprintln!("[pdf] recovery copy move failed: {}", error.message);
        }
        let acknowledged = authority.effect(&mut || {
            if let (Some(recovery), Ok(true)) = (&recovery, &moved) { completed.rec.borrow_mut().finish_renamed(view, recovery); }
            if watched && completed.services.views.sender_of(view).is_some() {
                completed.stop_view_watch(view);
                completed.watch_view_file(view, &new);
            }
            Ok(())
        });
        if let Err(error) = acknowledged { return then(Err(kit::error(error))); }
        let delivered = authority.emit("pdf:file-renamed", vec![new.clone().into_ipc()]);
        if let Err(error) = delivered { return then(Err(kit::error(error))); }
        if !alias {
            if let RenameOrigin::Native(snapshot, expected) = &authority.origin {
                if snapshot.selection().path() != expected { return then(Err(kit::error("PDF native save destination changed"))); }
                let destination = &authority.participants[0];
                let Some(document) = destination.pdf_document().cloned() else { return then(Err(kit::error("PDF document unavailable"))); };
                let Some(previous) = destination.pdf_revision().cloned() else { return then(Err(kit::error("PDF revision unavailable"))); };
                let successor = shell_contract::ipc::PdfDocumentLease::new();
                let mut prepared = match snapshot.prepare_with_request_aliases(vec![(previous.clone(), successor.clone())], &authority.participants) {
                    Ok(prepared) => prepared,
                    Err(error) => return then(Err(kit::error(error))),
                };
                let receipt = std::cell::RefCell::new(None);
                let done = std::cell::RefCell::new(Some(then));
                let mut before_notify = || {
                    if let (Some(done), Some(receipt)) = (done.borrow_mut().take(), receipt.borrow_mut().take()) {
                        done(Ok(Some(receipt)));
                    }
                };
                let additional: Vec<_> = authority.additional.iter().collect();
                let result = authority.effects.with_native_save_revision_transition(
                    &mut prepared, &authority.participants, &authority.cancel, &authority.pending, &additional, &mut before_notify,
                    &mut |admitted, prepared| {
                        let mut state = completed.st.borrow_mut();
                        let slot = state.views.get_mut(&view).ok_or("PDF document unavailable")?;
                        if !slot.document_lease.as_ref().is_some_and(|lease| lease.same_document(&document))
                            || !slot.document_revision.as_ref().is_some_and(|revision| revision.same_document(&previous)) {
                            return Err("PDF document changed before rename revision transition".into());
                        }
                        prepared.commit(admitted)?;
                        slot.document_revision = Some(successor.clone());
                        *receipt.borrow_mut() = Some(prepared.receipt().expect("committed PDF revision receipt"));
                        Ok(())
                    },
                );
                drop(before_notify);
                if let Some(done) = done.into_inner() {
                    done(Err(kit::error(result.err().unwrap_or_else(|| "PDF revision transition did not deliver its receipt".into()))));
                }
                return;
            }
            completed.advance_document_revision(view);
        }
        then(Ok(None));
    });
}

#[cfg(test)]
mod tests;
