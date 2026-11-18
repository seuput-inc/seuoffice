use std::{cell::RefCell, rc::Rc, sync::{Arc, Mutex}};
use shell_contract::{IpcValue, Surface, ViewId, editors::DocumentEditors, hosts::{OwnedEffects, OwnedEffectsHost}, ipc::{NativeContext, NativeRequestOwner}, runtime::{CancelToken, RendererGeneration, Workers}};
use crate::{kit::{IntoIpc, field, obj}, testing::Rt};
use super::super::{PdfEditors, fakes, hosts::{PdfEngineHost, PdfShellHost}};

#[derive(Default)]
struct Effects { gate: Mutex<()>, events: Mutex<Vec<Vec<IpcValue>>> }
impl Effects {
    fn live(owner: &NativeRequestOwner) -> bool {
        owner.context.renderer().is_some_and(|renderer| renderer.is_active())
            && owner.loss_token().is_some_and(|loss| !loss.is_cancelled())
            && owner.pdf_document().is_none_or(|lease| lease.is_live())
            && owner.pdf_revision().is_none_or(|lease| lease.is_live())
    }
}
impl OwnedEffects for Effects {
    fn docs_document_identity(&self, _owner: &NativeRequestOwner) -> Option<(ViewId, u64)> { None }
    fn with_exact_browser_owner_effect(&self, _: &shell_contract::ipc::NativeBrowserOwner, _: &NativeRequestOwner, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Browser ownership is not scripted in the PDF save fixture".into()) }
    fn with_native_saved_target_seed(&self, _: &shell_contract::ipc::NativeSavedTarget, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>) -> Result<(), String> { Err("Saved input is not scripted in the PDF save fixture".into()) }
    fn with_native_saved_document_input(&self, _: &shell_contract::ipc::NativeSavedDocument, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>) -> Result<(), String> { Err("Saved input is not scripted in the PDF save fixture".into()) }
    fn emit_native_saved_document_input(&self, _: &shell_contract::ipc::NativeSavedDocument, _: &[NativeRequestOwner], _: usize, _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &str, _: Vec<IpcValue>) -> Result<(), String> { Err("Saved input is not scripted in the PDF save fixture".into()) }
    fn with_native_saved_document_same_origin(&self, _: &shell_contract::ipc::NativeSavedDocument, _: &NativeRequestOwner, _: &CancelToken, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>) -> Result<(), String> { Err("Saved input is not scripted in the PDF save fixture".into()) }
    fn emit_native_saved_document_same_origin(&self, _: &shell_contract::ipc::NativeSavedDocument, _: &NativeRequestOwner, _: &CancelToken, _: &[NativeRequestOwner], _: usize, _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &str, _: Vec<IpcValue>) -> Result<(), String> { Err("Saved input is not scripted in the PDF save fixture".into()) }
    fn with_native_saved_document_transition(&self, _: &shell_contract::ipc::NativeSavedDocument, _: &NativeRequestOwner, _: &CancelToken, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput, &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>) -> Result<(), String>) -> Result<(), String> { Err("Saved input is not scripted in the PDF save fixture".into()) }
    fn with_native_command_preparation_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native command preparation is not scripted in the PDF fixture".into()) }
    fn with_startup_command_creation_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &NativeRequestOwner, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand, _: &RendererGeneration, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Startup command creation is not scripted in the PDF fixture".into()) }
    fn with_startup_command_target_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &NativeRequestOwner, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand, _: &shell_contract::ipc::NativeInvocationDocument, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Startup command target is not scripted in the PDF fixture".into()) }
    fn with_startup_command_preparation_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &NativeRequestOwner, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Startup command preparation is not scripted in the PDF fixture".into()) }
    fn with_startup_recovery_creation_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &NativeRequestOwner, _: &shell_contract::ipc::NativeInvocationOperation, _: &RendererGeneration, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Startup recovery creation is not scripted in the PDF fixture".into()) }
    fn with_startup_recovery_seed_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &NativeRequestOwner, _: &shell_contract::ipc::NativeInvocationTarget, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Startup recovery seed is not scripted in the PDF fixture".into()) }
    fn with_native_command_creation_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand, _: &RendererGeneration, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native command creation is not scripted in the PDF fixture".into()) }
    fn with_native_command_target_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand, _: &shell_contract::ipc::NativeInvocationDocument, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native command target is not scripted in the PDF fixture".into()) }
    fn settle_recovery_seed_snapshot(&self, _: &shell_contract::ipc::RecoverySeedReceipt, _: &shell_contract::ipc::RecoveryAuthoritySnapshot, _: &NativeRequestOwner, _: &CancelToken, _: Result<(), String>) -> Result<(), String> { Err("Recovery settlement is not scripted in the PDF fixture".into()) }
    fn emit_recovery_open_snapshot(&self, _: &shell_contract::ipc::RecoverySeedReceipt, _: &shell_contract::ipc::RecoveryAuthoritySnapshot, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: usize, _: &CancelToken, _: &CancelToken, _: &str, _: Vec<shell_contract::IpcValue>) -> Result<(), String> { Err("Recovery events are not scripted in the PDF fixture".into()) }
    fn with_recovery_dialog_snapshot(&self, _: &shell_contract::ipc::RecoveryOpenTarget, _: &shell_contract::ipc::RecoveryAuthoritySnapshot, _: &NativeRequestOwner, _: &RendererGeneration, _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Recovery dialog is not scripted in the PDF fixture".into()) }
    fn emit_recovery_dialog_snapshot(&self, _: &shell_contract::ipc::RecoveryOpenTarget, _: &shell_contract::ipc::RecoveryAuthoritySnapshot, _: &NativeRequestOwner, _: &RendererGeneration, _: &CancelToken, _: &CancelToken, _: &str, _: Vec<shell_contract::IpcValue>) -> Result<(), String> { Err("Recovery dialog events are not scripted in the PDF fixture".into()) }
    fn post_ui_existing_document_save(&self, _: shell_contract::ipc::NativeExistingDocumentBinding, _: Vec<NativeRequestOwner>, _: CancelToken, _: CancelToken, _: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>) { done(Err("Native existing-document save is not scripted in the PDF fixture".into())); }
    fn with_existing_document_save_effect(&self, binding: &shell_contract::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { shell_contract::hosts::synthetic::with_existing_document_save(binding, participants, cancel, pending, effect) }
    fn with_existing_document_save_effect_extra(&self, binding: &shell_contract::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { shell_contract::hosts::synthetic::with_existing_document_save_extra(binding, participants, cancel, pending, additional, effect) }
    fn with_existing_document_same_origin_effect(&self, _: &shell_contract::ipc::NativeExistingDocumentBinding, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native existing-document save is not scripted in the PDF fixture".into()) }
    fn emit_existing_document_save_many_extra(&self, binding: &shell_contract::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], _: &str, args: Vec<IpcValue>) -> Result<(), String> {
        participants.get(target).ok_or("synthetic event target unavailable")?;
        let mut args = Some(args);
        shell_contract::hosts::synthetic::with_existing_document_save_extra(binding, participants, cancel, pending, additional, &mut || { self.events.lock().unwrap().push(args.take().ok_or("event already emitted")?); Ok(()) })
    }
    fn post_ui_existing_document_save_extra(&self, _: shell_contract::ipc::NativeExistingDocumentBinding, _: Vec<NativeRequestOwner>, _: CancelToken, _: CancelToken, _: Vec<CancelToken>, _: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>) { done(Err("Native existing-document save is not scripted in the PDF fixture".into())); }
    fn with_owned_effect_many_extra(&self, origin: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { shell_contract::hosts::synthetic::with_owned_effect_many_extra(origin, participants, cancel, pending, additional, effect) }
    fn emit_owned_many_extra(&self, origin: &NativeRequestOwner, participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], _: &str, args: Vec<IpcValue>) -> Result<(), String> {
        participants.get(target).ok_or("synthetic event target unavailable")?;
        let mut args = Some(args);
        shell_contract::hosts::synthetic::with_owned_effect_many_extra(origin, participants, cancel, pending, additional, &mut || { self.events.lock().unwrap().push(args.take().ok_or("event already emitted")?); Ok(()) })
    }
    fn post_ui_owned_many_extra(&self, _: NativeRequestOwner, _: Vec<NativeRequestOwner>, _: CancelToken, _: CancelToken, _: Vec<CancelToken>, _: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>) { done(Err("Extra-cancellation UI is not scripted in the PDF fixture".into())); }
    fn with_native_invocation_effect_extra(&self, _: &shell_contract::ipc::NativeInvocationDocument, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native invocation is not scripted in the PDF fixture".into()) }
    fn with_saved_reload_acceptance_effect(&self, _: &shell_contract::ipc::NativeSavedInput, _: &shell_contract::ipc::NativeSavedPath, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native save reload is not scripted in the PDF fixture".into()) }
    fn with_native_save_reload_effect(&self, _: &shell_contract::hosts::NativeSaveReloadReceipt, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native save reload is not scripted in the PDF fixture".into()) }
    fn with_native_save_reload_parser_effect(&self, _: &shell_contract::hosts::NativeSaveReloadHandoff, _: &NativeRequestOwner, _: &CancelToken, _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>) -> Result<(), String> { Err("Native save reload is not scripted in the PDF fixture".into()) }
    fn with_native_save_reload_reader_effect(&self, _: &shell_contract::hosts::NativeSaveReloadReceipt, _: &NativeRequestOwner, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native save reload is not scripted in the PDF fixture".into()) }
    fn with_native_save_reload_many_effect_extra(&self, _: &shell_contract::hosts::NativeSaveReloadReceipt, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Native save reload is not scripted in the PDF fixture".into()) }
    fn stage_saved_docs_media(&self, _: &shell_contract::ipc::NativeSavedDocument, _: &NativeRequestOwner, _: &CancelToken, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &dyn shell_contract::hosts::DocsMediaStage) -> Result<(), String> { Err("Saved Docs media is not scripted in the PDF fixture".into()) }
    fn post_ui_native_saved_document_input(&self, _: shell_contract::ipc::NativeSavedDocument, _: Vec<NativeRequestOwner>, _: CancelToken, _: CancelToken, _: Vec<CancelToken>, _: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>) { done(Err("Saved Docs UI is not scripted in the PDF fixture".into())); }
    fn with_prepared_saved_publication(&self, _: &shell_contract::ipc::SavedPublication, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut(&mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>) -> Result<(), String>) -> Result<(), String> { Err("Saved publication is not scripted in the PDF fixture".into()) }
    fn transition_saved_docs_media(&self, _: &shell_contract::ipc::NativeSavedDocument, _: &NativeRequestOwner, _: &CancelToken, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &dyn shell_contract::hosts::DocsMediaTransition, _: &mut dyn FnMut(&NativeRequestOwner, &mut dyn FnMut() -> Result<(), String>) -> Result<(), String>, _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedDocument, &NativeRequestOwner) -> Result<(), String>) -> Result<(shell_contract::ipc::NativeSavedDocument, NativeRequestOwner), String> { Err("Saved Docs media is not scripted in the PDF fixture".into()) }
    fn with_native_save_reload_completion_effect(&self, _: &shell_contract::hosts::NativeSaveReloadHandoff, _: &NativeRequestOwner, _: &mut dyn FnMut(), _: &mut dyn FnMut(&mut shell_contract::ipc::AdmittedPdfReloadRetirement<'_, '_>) -> Result<(), String>) -> Result<(), String> { Err("Native save reload is not scripted in the PDF fixture".into()) }
    fn with_native_save_revision_transition(&self, prepared: &mut shell_contract::ipc::PreparedNativeSaveRevision, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], before_notify: &mut dyn FnMut(), effect: &mut dyn FnMut(&mut shell_contract::ipc::AdmittedPdfRevisionSet<'_, '_>, &mut shell_contract::ipc::PreparedNativeSaveRevision) -> Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::with_native_save_revision_transition(prepared, participants, cancel, pending, additional, before_notify, effect)
    }
    fn emit_native_save_revision_snapshot(&self, snapshot: &shell_contract::ipc::NativeSaveRevisionSnapshot, participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], _: &str, args: Vec<IpcValue>) -> Result<(), String> {
        participants.get(target).ok_or("synthetic event target unavailable")?;
        let mut args = Some(args);
        self.with_native_save_revision_snapshot_effect(snapshot, participants, cancel, pending, additional, &mut || {
            self.events.lock().unwrap().push(args.take().ok_or("event already emitted")?);
            Ok(())
        })
    }
    fn post_ui_native_save_revision_snapshot(&self, snapshot: shell_contract::ipc::NativeSaveRevisionSnapshot, participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken, additional: Vec<CancelToken>, action: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let mut action = Some(action);
        let mut accepted = None;
        let extra: Vec<_> = additional.iter().collect();
        let result = self.with_native_save_revision_snapshot_effect(&snapshot, &participants, &cancel, &pending, &extra, &mut || { accepted = action.take(); Ok(()) });
        done(result.and_then(|()| accepted.ok_or_else(|| "synthetic UI action was not admitted".to_owned())?()));
    }
    fn emit_existing_document_save_many(&self, binding: &shell_contract::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken, channel: &str, args: Vec<IpcValue>) -> Result<(), String> { self.emit_existing_document_save_many_extra(binding, participants, target, cancel, pending, &[], channel, args) }
    fn with_recovery_seed_snapshot(&self, _: &shell_contract::ipc::RecoveryOpenTarget, _: &shell_contract::ipc::RecoveryAuthoritySnapshot, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Recovery seed is not scripted in the PDF fixture".into()) }
    fn commit_recovery_seed_snapshot(&self, _: &shell_contract::ipc::RecoveryOpenTarget, _: &shell_contract::ipc::RecoveryAuthoritySnapshot, _: &mut dyn FnMut() -> Result<(), String>) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> { Err("Recovery seed is not scripted in the PDF fixture".into()) }
    fn with_recovery_open_snapshot(&self, _: &shell_contract::ipc::RecoverySeedReceipt, _: &shell_contract::ipc::RecoveryAuthoritySnapshot, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Recovery open is not scripted in the PDF fixture".into()) }
    fn with_command_recovery_target_effect(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeInvocationDocument, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Command recovery is not scripted in the PDF fixture".into()) }
    fn with_recovery_pdf_revision_transition(&self, _: &shell_contract::ipc::RecoverySeedReceipt, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &shell_contract::ipc::PdfDocumentLease, _: &shell_contract::ipc::PdfDocumentLease, _: &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal, _: &mut dyn FnMut(), _: &mut dyn FnMut(&mut shell_contract::ipc::AdmittedPdfRevisionRetirement<'_, '_>, &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal) -> Result<(), String>) -> Result<(), String> { Err("Recovery PDF revision transition is not scripted in the PDF fixture".into()) }
    fn with_recovery_open_effect_many_extra(&self, _: &shell_contract::ipc::RecoverySeedReceipt, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &[&CancelToken], _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Recovery open is not scripted in the PDF fixture".into()) }
    fn with_recovery_open_effect_many(&self, _: &shell_contract::ipc::RecoverySeedReceipt, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> { Err("Recovery open is not scripted in the PDF fixture".into()) }
    fn emit_native_command_creation_many(&self, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand, _: &RendererGeneration, _: &[NativeRequestOwner], _: usize, _: &CancelToken, _: &CancelToken, _: &str, _: Vec<shell_contract::IpcValue>) -> Result<(), String> { Err("Native command creation events are not scripted in the PDF fixture".into()) }
    fn emit_recovery_open_many(&self,_:&shell_contract::ipc::RecoverySeedReceipt,_:&NativeRequestOwner,_:&[NativeRequestOwner],_:usize,_:&CancelToken,_:&CancelToken,_:&str,_:Vec<shell_contract::IpcValue>)->Result<(),String> { Err("Recovery events are not scripted in the PDF fixture".into()) }
    fn stage_docs_media(&self, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &dyn shell_contract::hosts::DocsMediaStage, _: Option<&shell_contract::ipc::RecoverySeedReceipt>) -> Result<(), String> {
        Err("Docs media staging is not scripted in the PDF fixture".into())
    }
    fn with_startup_recovery_effect(&self, startup: &shell_contract::ipc::NativeInvocationOperation, shell: &NativeRequestOwner, child: &shell_contract::ipc::NativeInvocationDocument, owner: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::with_startup_recovery(startup, shell, child, owner, participants, cancel, pending, effect)
    }
    fn emit_startup_recovery_many(&self, startup: &shell_contract::ipc::NativeInvocationOperation, shell: &NativeRequestOwner, child: &shell_contract::ipc::NativeInvocationDocument, owner: &NativeRequestOwner, participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken, _channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        participants.get(target).ok_or("synthetic event target unavailable")?;
        let mut args = Some(args);
        shell_contract::hosts::synthetic::with_startup_recovery(startup, shell, child, owner, participants, cancel, pending, &mut || {
            self.events.lock().unwrap().push(args.take().ok_or("event already emitted")?); Ok(())
        })
    }
    fn transition_docs_media(&self, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &CancelToken, _: &CancelToken, _: &dyn shell_contract::hosts::DocsMediaTransition, _: Option<&shell_contract::ipc::RecoverySeedReceipt>, _: &mut dyn FnMut(&NativeRequestOwner, &mut dyn FnMut() -> Result<(), String>) -> Result<(), String>) -> Result<NativeRequestOwner, String> {
        Err("Docs media transition is not scripted in the PDF fixture".into())
    }
    fn with_native_operation_effect(&self, operation: &shell_contract::ipc::NativeInvocationOperation, origin: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::with_native_operation(operation, origin, participants, cancel, pending, effect)
    }
    fn emit_native_operation_many(&self, operation: &shell_contract::ipc::NativeInvocationOperation, origin: &NativeRequestOwner, participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken, _channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        participants.get(target).ok_or("synthetic event target unavailable")?;
        let mut args = Some(args);
        shell_contract::hosts::synthetic::with_native_operation(operation, origin, participants, cancel, pending, &mut || {
            self.events.lock().unwrap().push(args.take().ok_or("event already emitted")?); Ok(())
        })
    }
    fn with_native_invocation_effect(&self, document: &shell_contract::ipc::NativeInvocationDocument, origin: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::with_native_invocation(document, origin, participants, cancel, pending, effect)
    }
    fn emit_native_invocation_many(&self, document: &shell_contract::ipc::NativeInvocationDocument, origin: &NativeRequestOwner, participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken, _channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        participants.get(target).ok_or("synthetic event target unavailable")?;
        let mut args = Some(args);
        shell_contract::hosts::synthetic::with_native_invocation(document, origin, participants, cancel, pending, &mut || {
            self.events.lock().unwrap().push(args.take().ok_or("event already emitted")?);
            Ok(())
        })
    }
    fn with_same_native_origin_effect(&self, expected: &NativeRequestOwner, actual: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::with_same_native_origin(expected, actual, participants, cancel, pending, additional, effect)
    }
    fn with_recovery_dialog_effect(&self, target: &shell_contract::ipc::RecoveryOpenTarget, dialog: &NativeRequestOwner, generation: &RendererGeneration, cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::with_recovery_dialog(target, dialog, generation, cancel, pending, effect)
    }
    fn emit_recovery_dialog(&self, target: &shell_contract::ipc::RecoveryOpenTarget, dialog: &NativeRequestOwner, generation: &RendererGeneration, cancel: &CancelToken, pending: &CancelToken, _channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        let mut args = Some(args);
        shell_contract::hosts::synthetic::with_recovery_dialog(target, dialog, generation, cancel, pending, &mut || {
            self.events.lock().unwrap().push(args.take().ok_or("event already emitted")?);
            Ok(())
        })
    }
    fn with_recovery_seed_effect(&self, target: &shell_contract::ipc::RecoveryOpenTarget, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::with_recovery_seed(target, effect)
    }
    fn commit_recovery_seed(&self, target: &shell_contract::ipc::RecoveryOpenTarget, enqueue: &mut dyn FnMut() -> Result<(), String>) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
        shell_contract::hosts::synthetic::commit_recovery_seed(target, enqueue)
    }
    fn with_recovery_open_effect_pending(&self, _: &shell_contract::ipc::RecoverySeedReceipt, _: &NativeRequestOwner, _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        Err("Pending recovery admission is not scripted in the PDF fixture".into())
    }
    fn with_recovery_open_effect(&self, receipt: &shell_contract::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner, cancel: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::with_recovery_open(receipt, receiver, cancel, effect)
    }
    fn settle_recovery_seed(&self, receipt: &shell_contract::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner, cancel: &CancelToken, result: Result<(), String>) -> Result<(), String> {
        shell_contract::hosts::synthetic::settle_recovery_seed(receipt, receiver, cancel, result)
    }
    fn with_owned_effect_many(
        &self, origin: &NativeRequestOwner, destinations: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.with_owned_transition_many(origin, destinations, cancel, pending, &mut |_| effect())
    }
    fn post_ui_recovery_open_many(&self, receipt: shell_contract::ipc::RecoverySeedReceipt, receiver: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let admitted = shell_contract::hosts::synthetic::with_recovery_open_many(&receipt, &receiver, &participants, &cancel, &pending, &mut || Ok(()));
        done(admitted.and_then(|()| action()));
    }
    fn post_ui_owned_many(
        &self, origin: NativeRequestOwner, destinations: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let admitted = self.with_owned_effect_many(&origin, &destinations, &cancel, &pending, &mut || Ok(()));
        done(admitted.and_then(|()| action()));
    }
    fn with_owned_transition(
        &self, origin: &NativeRequestOwner, destination: &NativeRequestOwner,
        cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut(&mut shell_contract::runtime::AdmittedDocumentRetirement<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        self.with_owned_transition_many(origin, std::slice::from_ref(destination), cancel, pending, effect)
    }
    fn with_owned_transition_many(
        &self, origin: &NativeRequestOwner, destinations: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut(&mut shell_contract::runtime::AdmittedDocumentRetirement<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let count = destinations.len() + 1;
        let mut generations = Vec::with_capacity(count * 2);
        let mut documents = Vec::with_capacity(count);
        let mut pdf = Vec::with_capacity(count * 2);
        let mut losses = Vec::with_capacity(count);
        for owner in std::iter::once(origin).chain(destinations) {
            generations.push(owner.context.renderer().ok_or("synthetic renderer unavailable")?);
            if let Some(document) = owner.context.document_lease() {
                generations.push(document);
                documents.push(document);
            }
            if let Some(document) = owner.pdf_document() { pdf.push(document); }
            if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
            losses.push(owner.loss_token().ok_or("synthetic owner loss unavailable")?);
        }
        let mut tokens = Vec::with_capacity(losses.len() + 2);
        tokens.push(cancel);
        tokens.push(pending);
        tokens.extend(losses.iter());
        let mut generations = shell_contract::runtime::RendererGenerationGatePlan::from_generations(generations);
        let mut retirement = generations.prepare_retirement(&documents)?;
        let mut pdf = shell_contract::ipc::PdfDocumentGatePlan::new(&pdf);
        let mut tokens = shell_contract::runtime::CancelTokenGatePlan::from_tokens(tokens);
        let result = {
            let _guard = self.gate.lock().unwrap();
            generations.with_active_retiring(&mut retirement, |admitted| {
                pdf.with_live(|| tokens.with_active(|| effect(admitted)))
            }).flatten().flatten().ok_or_else(|| "synthetic document ownership released".to_owned())
        };
        result?
    }
    fn emit_owned_many(
        &self, origin: &NativeRequestOwner, destinations: &[NativeRequestOwner], target: usize,
        cancel: &CancelToken, pending: &CancelToken, _channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        destinations.get(target).ok_or("synthetic event target unavailable")?;
        let mut args = Some(args);
        self.with_owned_transition_many(origin, destinations, cancel, pending, &mut |_| {
            let args = args.take().ok_or("event already emitted")?;
            self.events.lock().unwrap().push(args);
            Ok(())
        })
    }
    fn capture_owner(&self, view: ViewId, expected: &RendererGeneration) -> Result<NativeRequestOwner, String> {
        Ok(NativeRequestOwner { view, context: NativeContext::with_owner_loss((), expected.clone(), CancelToken::default()) })
    }
    fn with_created_many_effect(&self, _: &NativeRequestOwner, _: &[NativeRequestOwner], _: &RendererGeneration, _: &CancelToken, _: &CancelToken, _: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        Err("Multi-owner creation is not scripted in the PDF fixture".into())
    }
    fn with_created_effect(&self, source: &NativeRequestOwner, created: &RendererGeneration, cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        let _guard = self.gate.lock().unwrap();
        if !Self::live(source) || !created.is_active() || cancel.is_cancelled() || pending.is_cancelled() { return Err("synthetic owner revoked".into()); }
        effect()
    }
    fn with_owned_effect(&self, origin: &NativeRequestOwner, destination: &NativeRequestOwner, cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        let _guard = self.gate.lock().unwrap();
        if !Self::live(origin) || !Self::live(destination) || cancel.is_cancelled() || pending.is_cancelled() { return Err("synthetic owner revoked".into()); }
        effect()
    }
    fn emit_owned(&self, origin: &NativeRequestOwner, destination: &NativeRequestOwner, cancel: &CancelToken, pending: &CancelToken, _channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        self.with_owned_effect(origin, destination, cancel, pending, &mut || { self.events.lock().unwrap().push(args.clone()); Ok(()) })
    }
    fn post_ui_owned(&self, origin: NativeRequestOwner, destination: NativeRequestOwner, cancel: CancelToken, pending: CancelToken, action: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let result = self.with_owned_effect(&origin, &destination, &cancel, &pending, &mut || Ok(()));
        done(result.and_then(|()| action()));
    }
}
#[derive(Default)]
struct Deferred(Mutex<Vec<Box<dyn FnOnce() + Send>>>);
impl Workers for Deferred { fn spawn(&self, job: Box<dyn FnOnce() + Send>) { self.0.lock().unwrap().push(job); } }

#[test]
fn owned_pdf_save_requires_exact_reply_and_native_write_and_rejects_cancelled_publish() {
    for cancel_before_write in [false, true] {
        let effects = Arc::new(Effects::default());
        let installed = effects.clone();
        let mut rt = Rt::new(move |hosts| {
            hosts.insert::<PdfEngineHost>(fakes::engine(&serde_json::Value::Null));
            hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
            hosts.insert::<OwnedEffectsHost>(installed);
            hosts.insert(Arc::new(Mutex::new(main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"))));
            crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
        });
        let workers = Arc::new(Deferred::default());
        rt.services.workers = workers.clone();
        rt.register(crate::pdf::entries).unwrap();
        let sender = rt.view(1, Surface::Pdf);
        let source = rt.dir.join("source.pdf");
        let target = rt.dir.join("target.pdf");
        std::fs::write(&source, b"%PDF original").unwrap();
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
        editor.open_view(sender.view, source.to_str());
        let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
        let origin = effects.capture_owner(sender.view, &renderer).unwrap();
        let cancel = CancelToken::default();
        let completed = Rc::new(RefCell::new(None));
        let result = completed.clone();
        editor.save_to_path_owned(sender.view, target.clone(), origin.clone(), cancel.clone(), Box::new(move |answer| *result.borrow_mut() = Some(answer)));
        let callback_context = editor.0.st.borrow().views.get(&sender.view).unwrap().owned_save.as_ref().unwrap().destination.context.clone();
        let event = effects.events.lock().unwrap().pop().unwrap();
        let id = event[1].clone();
        rt.send("pdf:save-as-result", sender, vec![true.into_ipc(), id.clone()]);
        assert!(completed.borrow().is_none());
        let request = |id: IpcValue| obj([("path", source.to_string_lossy().into_owned().into_ipc()), ("targetPath", target.to_string_lossy().into_owned().into_ipc()), ("ownedSaveId", id)]);
        let wrong = rt.invoke_now("pdf:save", sender, vec![request("wrong".into_ipc())]).unwrap();
        assert_eq!(field(&wrong, "ok"), &false.into_ipc());
        assert!(!target.exists());
        let pending = rt.invoke_native("pdf:save", sender, vec![request(id.clone())], callback_context, CancelToken::default());
        if cancel_before_write { cancel.cancel(); }
        loop {
            let job = { let mut jobs = workers.0.lock().unwrap(); if jobs.is_empty() { None } else { Some(jobs.remove(0)) } };
            let Some(job) = job else { break };
            job(); rt.drain();
        }
        if cancel_before_write {
            assert!(!target.exists());
            assert!(completed.borrow().as_ref().unwrap().is_err());
            assert_eq!(field(&pending.take().unwrap().unwrap(), "ok"), &false.into_ipc());
        } else {
            assert_eq!(std::fs::read(&target).unwrap(), b"%PDF original");
            assert_eq!(field(&pending.take().unwrap().unwrap(), "ok"), &true.into_ipc());
            rt.send("pdf:save-as-result", sender, vec![true.into_ipc(), id]);
            assert!(completed.borrow().as_ref().unwrap().is_ok());
        }
    }
}

#[test]
fn native_pdf_save_binds_exact_path_and_respects_callback_cancellation() {
    for cancel_callback in [false, true] {
        let effects = Arc::new(Effects::default());
        let installed = effects.clone();
        let mut rt = Rt::new(move |hosts| {
            hosts.insert::<PdfEngineHost>(fakes::engine(&serde_json::Value::Null));
            hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
            hosts.insert::<OwnedEffectsHost>(installed);
            hosts.insert(Arc::new(Mutex::new(main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"))));
            crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
        });
        let workers = Arc::new(Deferred::default());
        rt.services.workers = workers.clone();
        rt.register(crate::pdf::entries).unwrap();
        let sender = rt.view(1, Surface::Pdf);
        let source = rt.dir.join("native-source.pdf");
        let target = rt.dir.join("native-target.pdf");
        let foreign = rt.dir.join("foreign.pdf");
        std::fs::write(&source, b"%PDF original").unwrap();
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
        editor.open_view(sender.view, source.to_str());
        let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
        let mut owner = effects.capture_owner(sender.view, &renderer).unwrap();
        owner.context = owner.context.with_document_lease(RendererGeneration::new());
        let owner = editor.capture_document_owner(sender.view, owner).unwrap();
        let issuer = shell_contract::ipc::NativeInvocationIssuer::new(CancelToken::default());
        let binding = issuer.issue_existing_document_save(Surface::Pdf, sender.view, shell_contract::ipc::NativeSaveSelection::ExactPath(target.clone()), owner.clone(), CancelToken::default()).unwrap();
        let rejected = Rc::new(RefCell::new(None));
        let out = rejected.clone();
        editor.save_to_path_native(sender.view, foreign.clone(), binding.clone(), CancelToken::default(), CancelToken::default(), Box::new(move |result| *out.borrow_mut() = Some(result)));
        assert!(rejected.borrow().as_ref().unwrap().is_err());
        assert!(!foreign.exists());
        let completed = Rc::new(RefCell::new(None));
        let out = completed.clone();
        editor.save_to_path_native(sender.view, target.clone(), binding, CancelToken::default(), CancelToken::default(), Box::new(move |result| *out.borrow_mut() = Some(result)));
        let event = effects.events.lock().unwrap().pop().unwrap();
        let id = event[1].clone();
        let callback_cancel = CancelToken::default();
        let request = obj([("path", source.to_string_lossy().into_owned().into_ipc()), ("targetPath", target.to_string_lossy().into_owned().into_ipc()), ("ownedSaveId", id.clone())]);
        let foreign_owner = effects.capture_owner(sender.view, &renderer).unwrap();
        let denied = rt.invoke_native("pdf:save", sender, vec![request.clone()], foreign_owner.context, CancelToken::default());
        assert_eq!(field(&denied.take().unwrap().unwrap(), "ok"), &false.into_ipc());
        assert!(!target.exists());
        let pending = rt.invoke_native("pdf:save", sender, vec![request], owner.context.clone(), callback_cancel.clone());
        if cancel_callback { callback_cancel.cancel(); }
        loop {
            let job = { let mut jobs = workers.0.lock().unwrap(); if jobs.is_empty() { None } else { Some(jobs.remove(0)) } };
            let Some(job) = job else { break };
            job(); rt.drain();
        }
        if cancel_callback {
            assert!(!target.exists());
            assert_eq!(field(&pending.take().unwrap().unwrap(), "ok"), &false.into_ipc());
        } else {
            assert_eq!(std::fs::read(&target).unwrap(), b"%PDF original");
            assert_eq!(field(&pending.take().unwrap().unwrap(), "ok"), &true.into_ipc());
            rt.send("pdf:save-as-result", sender, vec![true.into_ipc(), id]);
            assert!(completed.borrow().as_ref().unwrap().is_ok());
        }
    }
}

#[test]
fn missing_context_or_owned_host_never_enables_native_admission_legacy_save() {
    for (with_effects, with_context) in [(false, false), (false, true), (true, false), (true, true)] {
        let effects = Arc::new(Effects::default());
        let installed = effects.clone();
        let mut rt = Rt::without_synthetic_effects(move |hosts| {
            let engine = fakes::engine_requiring_native_authority(&serde_json::Value::Null);
            assert!(!super::super::hosts::PdfEngine::synthetic_save_capability(engine.as_ref()));
            hosts.insert::<PdfEngineHost>(engine);
            hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
            if with_effects { hosts.insert::<OwnedEffectsHost>(installed); }
            hosts.insert(Arc::new(Mutex::new(main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"))));
            crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
        });
        rt.register(crate::pdf::entries).unwrap();
        let sender = rt.view(1, Surface::Pdf);
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
        editor.open_view(sender.view, None);
        let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
        let context = with_context.then(|| effects.capture_owner(sender.view, &renderer).unwrap().context);
        let result = editor.0.direct_save_authority(sender.view, context, CancelToken::default());
        if with_effects && with_context {
            assert!(matches!(result, Ok(Some(_))));
        } else {
            assert!(result.is_err());
        }
    }
    // The legacy oracle is an explicit synthetic host capability, not a missing-owner fallback.
    let (mut rt, _) = super::super::tests::rt_with(serde_json::Value::Null);
    let sender = rt.view(1, Surface::Pdf);
    let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
    editor.open_view(sender.view, None);
    assert!(editor.0.direct_save_authority(sender.view, None, CancelToken::default()).is_err());
    let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
    let effects = rt.services.hosts.get::<OwnedEffectsHost>().unwrap();
    let native = effects.capture_owner(sender.view, &renderer).unwrap();
    assert!(matches!(editor.0.direct_save_authority(sender.view, Some(native.context), CancelToken::default()), Ok(Some(_))));
}

#[test]
fn owned_close_waits_for_its_renderer_and_does_not_cancel_parent_request() {
    let effects = Arc::new(Effects::default());
    let installed = effects.clone();
    let mut rt = Rt::new(move |hosts| {
        hosts.insert::<PdfEngineHost>(fakes::engine(&serde_json::Value::Null));
        hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
        hosts.insert::<OwnedEffectsHost>(installed);
        hosts.insert(Arc::new(Mutex::new(main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"))));
        crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
    });
    rt.register(crate::pdf::entries).unwrap();
    let sender = rt.view(1, Surface::Pdf);
    let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
    editor.open_view(sender.view, None);
    rt.send("pdf:dirty-changed", sender, vec![true.into_ipc()]);
    rt.script().message.push_back(0);
    let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
    let origin = effects.capture_owner(sender.view, &renderer).unwrap();
    let cancel = CancelToken::default();
    let completed = Rc::new(RefCell::new(None));
    let output = completed.clone();
    editor.request_close_owned(sender.view, None, origin, cancel.clone(), Box::new(move |result| *output.borrow_mut() = Some(result)));
    rt.drain();
    assert!(completed.borrow().is_none());
    let event = effects.events.lock().unwrap().pop().unwrap();
    let id = field(&event[0], "ownedSaveId").clone();
    assert_eq!(field(&event[0], "flushAll"), &true.into_ipc());
    rt.send("pdf:close-save-result", sender, vec![false.into_ipc(), id]);
    assert!(matches!(completed.borrow().as_ref(), Some(Ok(false))));
    assert!(!cancel.is_cancelled());
}

#[test]
fn native_revision_stage_rejects_changed_participants_before_retiring() {
    let (mut rt, _) = super::super::tests::rt_with(serde_json::Value::Null);
    let first = rt.view(1, Surface::Pdf);
    let second = rt.view(2, Surface::Pdf);
    let path = rt.dir.join("shared.pdf");
    std::fs::write(&path, b"%PDF shared").unwrap();
    let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
    editor.open_view(first.view, path.to_str());
    editor.open_view(second.view, path.to_str());
    let effects = rt.services.hosts.get::<OwnedEffectsHost>().unwrap();
    let renderer = rt.services.views.renderer_generation(first.view).unwrap();
    let owner = editor.capture_document_owner(first.view, effects.capture_owner(first.view, &renderer).unwrap()).unwrap();
    let stage = super::NativeSaveRevisionStage::prepare(&editor.0, first.view, path.to_str().unwrap(), &owner).unwrap();
    assert_eq!(stage.entries().len(), 2);
    let old = owner.pdf_revision().unwrap().clone();
    let rejected: Result<(), String> = stage.commit_admitted(&editor.0, || Err("retirement refused".into()));
    assert!(rejected.is_err());
    assert!(old.is_live());
    editor.0.advance_document_revision(second.view);
    let called = std::cell::Cell::new(false);
    let result = stage.commit_admitted(&editor.0, || { called.set(true); Ok(()) });
    assert!(result.is_err());
    assert!(!called.get());
    assert!(old.is_live());
}

#[test]
fn native_in_place_save_renews_binding_and_keeps_callback_live() {
    let mut rt = Rt::new(|hosts| {
        hosts.insert::<PdfEngineHost>(fakes::engine(&serde_json::Value::Null));
        hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
        hosts.insert(Arc::new(Mutex::new(main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"))));
        crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
    });
    let workers = Arc::new(Deferred::default());
    rt.services.workers = workers.clone();
    rt.register(crate::pdf::entries).unwrap();
    let sender = rt.view(1, Surface::Pdf);
    let path = rt.dir.join("native-in-place.pdf");
    std::fs::write(&path, b"%PDF original").unwrap();
    let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
    editor.open_view(sender.view, path.to_str());
    rt.send("pdf:dirty-changed", sender, vec![true.into_ipc()]);
    let generation = rt.services.views.renderer_generation(sender.view).unwrap();
    let native = rt.services.hosts.get::<OwnedEffectsHost>().unwrap().capture_owner(sender.view, &generation).unwrap();
    let context = native.context.clone().with_document_lease(RendererGeneration::new());
    let owner = editor.capture_document_owner(sender.view, NativeRequestOwner { view: sender.view, context }).unwrap();
    let old_revision = owner.pdf_revision().unwrap().clone();
    let issuer = shell_contract::ipc::NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer.issue_existing_document_save(Surface::Pdf, sender.view, shell_contract::ipc::NativeSaveSelection::Mode(shell_contract::editors::SaveMode::Save), owner.clone(), CancelToken::default()).unwrap();
    let completed = Rc::new(RefCell::new(None));
    let output = completed.clone();
    editor.request_save_native(sender.view, shell_contract::editors::SaveMode::Save, binding, CancelToken::default(), CancelToken::default(), Box::new(move |result| *output.borrow_mut() = Some(result)));
    let event = rt.events().into_iter().find(|event| event.channel == "pdf:close-save-request").unwrap();
    let id = field(&event.args[0], "ownedSaveId").clone();
    let callback_cancel = CancelToken::default();
    let request = obj([("path", path.to_string_lossy().into_owned().into_ipc()), ("ownedSaveId", id.clone())]);
    let pending = rt.invoke_native("pdf:save", sender, vec![request], owner.context.clone(), callback_cancel.clone());
    assert!(!old_revision.is_live());
    loop {
        let job = { let mut jobs = workers.0.lock().unwrap(); if jobs.is_empty() { None } else { Some(jobs.remove(0)) } };
        let Some(job) = job else { break };
        job(); rt.drain();
    }
    assert_eq!(field(&pending.take().unwrap().unwrap(), "ok"), &true.into_ipc());
    assert!(!callback_cancel.is_cancelled());
    rt.send("pdf:close-save-result", sender, vec![true.into_ipc(), id]);
    assert!(matches!(completed.borrow().as_ref(), Some(Ok(true))));
}
