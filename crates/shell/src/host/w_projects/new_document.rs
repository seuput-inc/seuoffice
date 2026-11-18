use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::CancelToken;
use shell_ipc_w::projects::Projects;
use shell_ipc_w::tabs::TabsState;

use crate::host::w::{on_core, post_core_checked};

enum ProjectWriteFailure {
    Message(String),
    Write(main_core::shell::projects::write_guard::WriteGuardError),
}

impl ProjectWriteFailure {
    fn message(&self) -> String {
        match self {
            Self::Message(error) => error.clone(),
            Self::Write(error) => error.message(),
        }
    }
}

/// Worker entry. Only the exclusive namespace publication holds native authority.
pub(crate) fn write_new_document_owned(
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
    effects: &OwnedEffectsHost,
    admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
    name: &str,
    bytes: &[u8],
) -> Result<Option<String>, String> {
    if crate::host::w::on_core_local(|_| {}) {
        return Err("Project document filesystem work must run on a worker".into());
    }
    effects.with_owned_effect(origin, destination, cancel, pending, &mut || Ok(()))?;
    let view = destination.view;
    let (seed, lookup, io, cleanup) = on_core(move |provided| {
        let tabs = provided.all::<TabsState>().into_iter().next().ok_or("The tab manager is not available")?;
        if tabs.details_of(view).is_none() { return Err("The document view is no longer open"); }
        let projects = provided.all::<Projects>().into_iter().next().ok_or("Project services are not available")?;
        let (seed, lookup, io) = projects.capture_new_document(view);
        Ok((seed, lookup, io, projects.new_document_cleanup()))
    }).ok_or("Core services are not available")?.map_err(str::to_owned)?;
    let operation = cleanup.begin()?;
    let queue = main_core::shell::app_settings::queue::for_path(io.settings_path());
    let name = name.to_owned();
    let bytes = bytes.to_vec();
    let origin = origin.clone();
    let destination = destination.clone();
    let cancel = cancel.clone();
    let pending = pending.clone();
    let effects = effects.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    queue.submit(move |completion| {
        let result = (|| -> Result<Option<String>, ProjectWriteFailure> {
            effects.with_owned_effect(&origin, &destination, &cancel, &pending, &mut || Ok(())).map_err(ProjectWriteFailure::Message)?;
            let Some(authority) = io.resolve(lookup).map_err(ProjectWriteFailure::Write)? else { return Ok(None); };
            let write = seed.resolve(authority.project_id().to_owned(), authority.root().to_owned(), &name).map_err(ProjectWriteFailure::Write)?;
            write.execute(&bytes, |_, target, bytes| authority.write(target, bytes, admission.clone())).map(Some).map_err(ProjectWriteFailure::Write)
        })();
        #[cfg(unix)]
        if let Err(ProjectWriteFailure::Write(error @ main_core::shell::projects::write_guard::WriteGuardError::Cleanup { cleanup: retained, .. })) = &result {
            let message = error.message();
            cleanup.retain(retained.clone(), completion, error.clone());
            drop(operation);
            let unavailable = tx.clone();
            post_core_checked(move |_, _| { let _ = tx.send(Err(message)); }, move || {
                let _ = unavailable.send(Err("Core services disappeared before the project save acknowledgment".into()));
            });
            if let Err(error) = cleanup.retry_for_successor() {
                eprintln!("shell: project cleanup retained for retry: {error}");
            }
            return;
        }
        let result = result.map_err(|error| error.message());
        let unavailable = tx.clone();
        post_core_checked(move |_, _| {
            drop(completion);
            drop(operation);
            let _ = tx.send(result);
        }, move || {
            let _ = unavailable.send(Err("Core services disappeared before the project save acknowledgment".into()));
        });
    });
    rx.recv().map_err(|_| "Project document save did not complete".to_owned())?
}
