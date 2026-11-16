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

type Admit = Box<dyn Fn() -> Result<(), String> + Send>;

/// Worker entry shared by every new-document write. Only the exclusive namespace publication
/// holds native authority; `admit` is the caller's canonical admission, run before the capture
/// and again inside the project settings queue; `write` performs the guarded write.
fn project_write<R: Send + 'static>(
    view: shell_contract::ids::ViewId,
    admit: Admit,
    name: &str,
    bytes: &[u8],
    write: impl FnOnce(
        &main_core::shell::projects::new_documents::NewDocumentWrite,
        &main_core::shell::projects::project_guide::NewDocumentAuthority,
        &[u8],
    ) -> Result<R, main_core::shell::projects::write_guard::WriteGuardError>
    + Send
    + 'static,
) -> Result<Option<R>, String> {
    if crate::host::w::on_core_local(|_| {}) {
        return Err("Project document filesystem work must run on a worker".into());
    }
    admit()?;
    let (seed, lookup, io, cleanup) = on_core(move |provided| {
        let tabs = provided
            .all::<TabsState>()
            .into_iter()
            .next()
            .ok_or("The tab manager is not available")?;
        if tabs.details_of(view).is_none() {
            return Err("The document view is no longer open");
        }
        let projects = provided
            .all::<Projects>()
            .into_iter()
            .next()
            .ok_or("Project services are not available")?;
        let (seed, lookup, io) = projects.capture_new_document(view);
        Ok((seed, lookup, io, projects.new_document_cleanup()))
    })
    .ok_or("Core services are not available")?
    .map_err(str::to_owned)?;
    let operation = cleanup.begin()?;
    let queue = main_core::shell::app_settings::queue::for_path(io.settings_path());
    let name = name.to_owned();
    let bytes = bytes.to_vec();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    queue.submit(move |completion| {
        let result = (|| -> Result<Option<R>, ProjectWriteFailure> {
            admit().map_err(ProjectWriteFailure::Message)?;
            let Some(authority) = io.resolve(lookup).map_err(ProjectWriteFailure::Write)? else {
                return Ok(None);
            };
            let plan = seed
                .resolve(
                    authority.project_id().to_owned(),
                    authority.root().to_owned(),
                    &name,
                )
                .map_err(ProjectWriteFailure::Write)?;
            write(&plan, &authority, &bytes)
                .map(Some)
                .map_err(ProjectWriteFailure::Write)
        })();
        #[cfg(unix)]
        if let Err(ProjectWriteFailure::Write(
            error @ main_core::shell::projects::write_guard::WriteGuardError::Cleanup {
                cleanup: retained,
                ..
            },
        )) = &result
        {
            let message = error.message();
            cleanup.retain(retained.clone(), completion, error.clone());
            drop(operation);
            let unavailable = tx.clone();
            post_core_checked(
                move |_, _| {
                    let _ = tx.send(Err(message));
                },
                move || {
                    let _ = unavailable.send(Err(
                        "Core services disappeared before the project save acknowledgment".into(),
                    ));
                },
            );
            if let Err(error) = cleanup.retry_for_successor() {
                eprintln!("shell: project cleanup retained for retry: {error}");
            }
            return;
        }
        let result = result.map_err(|error| error.message());
        let unavailable = tx.clone();
        post_core_checked(
            move |_, _| {
                drop(completion);
                drop(operation);
                let _ = tx.send(result);
            },
            move || {
                let _ = unavailable.send(Err(
                    "Core services disappeared before the project save acknowledgment".into(),
                ));
            },
        );
    });
    rx.recv()
        .map_err(|_| "Project document save did not complete".to_owned())?
}

type Admission = main_core::packages::electron_utils::atomic_write::owned::Admission;

/// Worker entry: the project path of a new document a renderer asked for.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_new_document_owned(
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
    effects: &OwnedEffectsHost,
    admission: Admission,
    name: &str,
    bytes: &[u8],
) -> Result<Option<String>, String> {
    let view = destination.view;
    let (origin, destination, cancel, pending, effects) = (
        origin.clone(),
        destination.clone(),
        cancel.clone(),
        pending.clone(),
        effects.clone(),
    );
    let admit: Admit = Box::new(move || {
        effects.with_owned_effect(&origin, &destination, &cancel, &pending, &mut || Ok(()))
    });
    project_write(view, admit, name, bytes, move |plan, authority, bytes| {
        plan.execute(bytes, |_, target, bytes| {
            authority.write(target, bytes, admission.clone())
        })
    })
}

/// Worker entry: the project path of a new document a native operation saves.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_new_document_existing_document(
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
    effects: &OwnedEffectsHost,
    admission: Admission,
    name: &str,
    bytes: &[u8],
) -> Result<Option<String>, String> {
    let view = binding.owner().view;
    let (binding, participants, cancel, pending, additional, effects) = (
        binding.clone(),
        participants.to_vec(),
        cancel.clone(),
        pending.clone(),
        additional.to_vec(),
        effects.clone(),
    );
    let admit: Admit = Box::new(move || {
        let extra: Vec<&CancelToken> = additional.iter().collect();
        effects.with_existing_document_save_effect_extra(
            &binding,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || Ok(()),
        )
    });
    project_write(view, admit, name, bytes, move |plan, authority, bytes| {
        plan.execute(bytes, |_, target, bytes| {
            authority.write(target, bytes, admission.clone())
        })
    })
}

/// Worker entry: the project path of a new document as a real publication and the bytes read
/// back from that exact commit, or `None` when the panel has no project.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_new_document_publication_owned(
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
    effects: &OwnedEffectsHost,
    admission: Admission,
    name: &str,
    bytes: &[u8],
) -> Result<
    Option<(
        shell_contract::ipc::SavedPublication,
        shell_contract::ipc::NativeSavedInput,
    )>,
    String,
> {
    let (admit_origin, admit_destination, admit_cancel, admit_pending, admit_effects) = (
        origin.clone(),
        destination.clone(),
        cancel.clone(),
        pending.clone(),
        effects.clone(),
    );
    let admit: Admit = Box::new(move || {
        admit_effects.with_owned_effect(
            &admit_origin,
            &admit_destination,
            &admit_cancel,
            &admit_pending,
            &mut || Ok(()),
        )
    });
    let committed = project_write(
        destination.view,
        admit,
        name,
        bytes,
        move |plan, authority, bytes| {
            plan.execute_with_receipt(bytes, |_, target, bytes| {
                authority.write_with_receipt(target, bytes, admission.clone())
            })
        },
    )?;
    let Some(receipt) = committed else {
        return Ok(None);
    };
    let path = receipt.path().to_owned();
    let publication = shell_contract::ipc::SavedPublication::renderer_commit(
        effects.as_ref(),
        origin,
        destination,
        participants,
        cancel,
        pending,
        additional,
        &path,
        receipt,
    )?;
    let input = publication
        .read_input(bytes.len(), participants, cancel, pending, additional)
        .map_err(|error| error.to_string())?;
    Ok(Some((publication, input)))
}
