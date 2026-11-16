//! Native menu, OS and authenticated service command issuance.
use std::sync::OnceLock;

use shell_contract::ipc::{NativeEditorCommand, NativeInvocationIssuer, NativeInvocationOperation};
use shell_contract::runtime::CancelToken;

struct Factory {
    issuer: NativeInvocationIssuer,
    shutdown: CancelToken,
}

impl Factory {
    fn new(shutdown: CancelToken) -> Self {
        Self {
            issuer: NativeInvocationIssuer::new(shutdown.clone()),
            shutdown,
        }
    }

    fn issue(
        &self,
        command: NativeEditorCommand,
        cancel: CancelToken,
    ) -> Result<NativeInvocationOperation, String> {
        if self.shutdown.is_cancelled() || cancel.is_cancelled() {
            return Err("The native command is no longer available.".into());
        }
        let operation = self.issuer.issue_command(command, cancel);
        operation
            .with_active(|| ())
            .ok_or("The native command is no longer available.")?;
        Ok(operation)
    }

    fn issue_existing_save(
        &self,
        surface: shell_contract::Surface,
        view: shell_contract::ViewId,
        selection: shell_contract::ipc::NativeSaveSelection,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: CancelToken,
    ) -> Result<shell_contract::ipc::NativeExistingDocumentBinding, String> {
        if self.shutdown.is_cancelled() || cancel.is_cancelled() {
            return Err("The native command is no longer available.".into());
        }
        self.issuer
            .issue_existing_document_save(surface, view, selection, owner, cancel)
    }

    fn issue_child(
        &self,
        startup: &NativeInvocationOperation,
        command: NativeEditorCommand,
        cancel: CancelToken,
    ) -> Result<NativeInvocationOperation, String> {
        if self.shutdown.is_cancelled() || cancel.is_cancelled() {
            return Err("The native command is no longer available.".into());
        }
        self.issuer.issue_command_child(startup, command, cancel)
    }

    fn issue_recovery_child(
        &self,
        command: &NativeInvocationOperation,
        selection: shell_contract::ipc::NativeInvocationSelection,
        cancel: CancelToken,
    ) -> Result<NativeInvocationOperation, String> {
        if self.shutdown.is_cancelled() || cancel.is_cancelled() {
            return Err("The native recovery command is no longer available.".into());
        }
        self.issuer
            .issue_command_recovery_child(command, selection, cancel)
    }
}

static FACTORY: OnceLock<Factory> = OnceLock::new();

/// Main installs the one process factory with the actual shutdown lifetime.
pub(crate) fn install(shutdown: CancelToken) -> Result<(), String> {
    FACTORY
        .set(Factory::new(shutdown))
        .map_err(|_| "The native command factory is already installed.".into())
}

/// Only native host entry points may issue; this is not installed in Services or IPC.
pub(super) fn issue(
    command: NativeEditorCommand,
    cancel: CancelToken,
) -> Result<NativeInvocationOperation, String> {
    FACTORY
        .get()
        .ok_or("The native command factory is not installed.")?
        .issue(command, cancel)
}

pub(super) fn issue_existing_save(
    surface: shell_contract::Surface,
    view: shell_contract::ViewId,
    selection: shell_contract::ipc::NativeSaveSelection,
    owner: shell_contract::ipc::NativeRequestOwner,
    cancel: CancelToken,
) -> Result<shell_contract::ipc::NativeExistingDocumentBinding, String> {
    FACTORY
        .get()
        .ok_or("The native command factory is not installed.")?
        .issue_existing_save(surface, view, selection, owner, cancel)
}

/// Only a trusted native producer may bind its selected command to startup.
pub(super) fn issue_child(
    startup: &NativeInvocationOperation,
    command: NativeEditorCommand,
    cancel: CancelToken,
) -> Result<NativeInvocationOperation, String> {
    FACTORY
        .get()
        .ok_or("The native command factory is not installed.")?
        .issue_child(startup, command, cancel)
}

/// The cached recovery selection remains bound to its trusted native open.
pub(super) fn issue_recovery_child(
    command: &NativeInvocationOperation,
    selection: shell_contract::ipc::NativeInvocationSelection,
    cancel: CancelToken,
) -> Result<NativeInvocationOperation, String> {
    FACTORY
        .get()
        .ok_or("The native command factory is not installed.")?
        .issue_recovery_child(command, selection, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_preserve_selection_and_both_cancellation_lifetimes() {
        let shutdown = CancelToken::default();
        let factory = Factory::new(shutdown.clone());
        let command = NativeEditorCommand::OpenHome;
        let cancel = CancelToken::default();
        let operation = factory.issue(command.clone(), cancel.clone()).unwrap();
        assert!(operation.is_native_command());
        assert_eq!(
            operation.selection(),
            &shell_contract::ipc::NativeInvocationSelection::NativeCommand(command)
        );
        cancel.cancel();
        assert!(operation.with_active(|| ()).is_none());
        shutdown.cancel();
        assert!(
            factory
                .issue(
                    NativeEditorCommand::OpenHelp { topic: None },
                    CancelToken::default()
                )
                .is_err()
        );
    }
}
