/// Explicit synthetic dialog fixture implementations, never used by the native host.
#[macro_export]
macro_rules! synthetic_recovery_dialog_methods {
    () => {
        fn message_recovery_owned(
            &self, parent: Option<$crate::WindowId>, options: $crate::hosts::MessageBox,
            receipt: $crate::ipc::RecoverySeedReceipt, snapshot: $crate::ipc::RecoveryAuthoritySnapshot,
            receiver: $crate::ipc::NativeRequestOwner,
            cancel: $crate::runtime::CancelToken, pending: $crate::runtime::CancelToken,
            done: Box<dyn FnOnce(Result<$crate::hosts::MessageBoxAnswer, String>) + Send>,
        ) {
            let admit = || $crate::hosts::synthetic::with_recovery_open_snapshot(&receipt, &snapshot, &receiver, &cancel, &pending, &mut || Ok(()));
            let result = admit().and_then(|()| {
                let answer = self.message(parent, &options);
                admit().map(|()| answer)
            });
            done(result);
        }
        fn save_recovery_owned(
            &self, parent: Option<$crate::WindowId>, options: $crate::hosts::FileDialog,
            receipt: $crate::ipc::RecoverySeedReceipt, snapshot: $crate::ipc::RecoveryAuthoritySnapshot,
            receiver: $crate::ipc::NativeRequestOwner,
            cancel: $crate::runtime::CancelToken, pending: $crate::runtime::CancelToken,
            done: Box<dyn FnOnce(Result<Option<$crate::ipc::RecoveryCommittedDestination>, String>) + Send>,
        ) {
            let admit = || $crate::hosts::synthetic::with_recovery_open_snapshot(&receipt, &snapshot, &receiver, &cancel, &pending, &mut || Ok(()));
            let result = admit().and_then(|()| {
                let mut selected = self.save(parent, &options);
                let mut result = None;
                $crate::hosts::synthetic::with_recovery_open_snapshot(&receipt, &snapshot, &receiver, &cancel, &pending, &mut || {
                    result = Some(selected.take().map(|path| $crate::ipc::RecoveryCommittedDestination::from_native_save_dialog(receipt.clone(), snapshot.clone(), path)).transpose()?);
                    Ok(())
                })?;
                result.ok_or_else(|| "Synthetic recovery selection was not admitted".to_owned())
            });
            done(result);
        }
        fn save_existing_document(
            &self, parent: Option<$crate::WindowId>, options: $crate::hosts::FileDialog,
            binding: $crate::ipc::NativeExistingDocumentBinding, participants: Vec<$crate::ipc::NativeRequestOwner>,
            cancel: $crate::runtime::CancelToken, pending: $crate::runtime::CancelToken, additional: Vec<$crate::runtime::CancelToken>,
            done: Box<dyn FnOnce(Result<Option<$crate::ipc::NativeSaveDestination>, String>) + Send>,
        ) {
            let extra: Vec<_> = additional.iter().collect();
            let admit = || $crate::hosts::synthetic::with_existing_document_save_extra(&binding, &participants, &cancel, &pending, &extra, &mut || Ok(()));
            let result = admit().and_then(|()| {
                let mut path = self.save(parent, &options);
                let mut result = None;
                $crate::hosts::synthetic::with_existing_document_save_extra(&binding, &participants, &cancel, &pending, &extra, &mut || {
                    result = Some(path.take().map(|path| $crate::ipc::NativeSaveDestination::from_native_save_dialog(binding.clone(), path)).transpose()?);
                    Ok(())
                })?;
                result.ok_or_else(|| "Synthetic native save selection was not admitted".to_owned())
            });
            done(result);
        }
        fn message_existing_document(
            &self, parent: Option<$crate::WindowId>, options: $crate::hosts::MessageBox,
            binding: $crate::ipc::NativeExistingDocumentBinding, participants: Vec<$crate::ipc::NativeRequestOwner>,
            cancel: $crate::runtime::CancelToken, pending: $crate::runtime::CancelToken, additional: Vec<$crate::runtime::CancelToken>,
            done: Box<dyn FnOnce(Result<$crate::hosts::MessageBoxAnswer, String>) + Send>,
        ) {
            let extra: Vec<_> = additional.iter().collect();
            let admit = || $crate::hosts::synthetic::with_existing_document_save_extra(&binding, &participants, &cancel, &pending, &extra, &mut || Ok(()));
            let result = admit().and_then(|()| {
                let answer = self.message(parent, &options);
                admit().map(|()| answer)
            });
            done(result);
        }
        fn save_existing_document_revision(
            &self, parent: Option<$crate::WindowId>, options: $crate::hosts::FileDialog,
            snapshot: $crate::ipc::NativeSaveRevisionSnapshot, participants: Vec<$crate::ipc::NativeRequestOwner>,
            cancel: $crate::runtime::CancelToken, pending: $crate::runtime::CancelToken, additional: Vec<$crate::runtime::CancelToken>,
            done: Box<dyn FnOnce(Result<Option<$crate::ipc::NativeSaveDestination>, String>) + Send>,
        ) {
            let extra: Vec<_> = additional.iter().collect();
            let admit = || $crate::hosts::synthetic::with_native_save_revision_snapshot(&snapshot, &participants, &cancel, &pending, &extra, &mut || Ok(()));
            let result = admit().and_then(|()| {
                let mut path = self.save(parent, &options);
                let mut result = None;
                $crate::hosts::synthetic::with_native_save_revision_snapshot(&snapshot, &participants, &cancel, &pending, &extra, &mut || {
                    result = Some(path.take().map(|path| $crate::ipc::NativeSaveDestination::from_native_save_dialog(snapshot.selection().binding().clone(), path)).transpose()?);
                    Ok(())
                })?;
                result.ok_or_else(|| "Synthetic native save revision selection was not admitted".to_owned())
            });
            done(result);
        }
        fn message_existing_document_revision(
            &self, parent: Option<$crate::WindowId>, options: $crate::hosts::MessageBox,
            snapshot: $crate::ipc::NativeSaveRevisionSnapshot, participants: Vec<$crate::ipc::NativeRequestOwner>,
            cancel: $crate::runtime::CancelToken, pending: $crate::runtime::CancelToken, additional: Vec<$crate::runtime::CancelToken>,
            done: Box<dyn FnOnce(Result<$crate::hosts::MessageBoxAnswer, String>) + Send>,
        ) {
            let extra: Vec<_> = additional.iter().collect();
            let admit = || $crate::hosts::synthetic::with_native_save_revision_snapshot(&snapshot, &participants, &cancel, &pending, &extra, &mut || Ok(()));
            let result = admit().and_then(|()| { let answer = self.message(parent, &options); admit().map(|()| answer) });
            done(result);
        }
        fn save_owned_many_extra(
            &self, parent: Option<$crate::WindowId>, options: $crate::hosts::FileDialog,
            origin: $crate::ipc::NativeRequestOwner, participants: Vec<$crate::ipc::NativeRequestOwner>,
            cancel: $crate::runtime::CancelToken, pending: $crate::runtime::CancelToken, additional: Vec<$crate::runtime::CancelToken>,
            done: Box<dyn FnOnce(Result<Option<std::path::PathBuf>, String>) + Send>,
        ) {
            let extra: Vec<_> = additional.iter().collect();
            let admit = || $crate::hosts::synthetic::with_owned_effect_many_extra(&origin, &participants, &cancel, &pending, &extra, &mut || Ok(()));
            let result = admit().and_then(|()| { let value = self.save(parent, &options); admit().map(|()| value) });
            done(result);
        }
        fn message_owned_many_extra(
            &self, parent: Option<$crate::WindowId>, options: $crate::hosts::MessageBox,
            origin: $crate::ipc::NativeRequestOwner, participants: Vec<$crate::ipc::NativeRequestOwner>,
            cancel: $crate::runtime::CancelToken, pending: $crate::runtime::CancelToken, additional: Vec<$crate::runtime::CancelToken>,
            done: Box<dyn FnOnce(Result<$crate::hosts::MessageBoxAnswer, String>) + Send>,
        ) {
            let extra: Vec<_> = additional.iter().collect();
            let admit = || $crate::hosts::synthetic::with_owned_effect_many_extra(&origin, &participants, &cancel, &pending, &extra, &mut || Ok(()));
            let result = admit().and_then(|()| { let value = self.message(parent, &options); admit().map(|()| value) });
            done(result);
        }
    };
}
