use main_core::packages::project_store::native_store::owned_rename::RenameCommit;
use shell_contract::{
    hosts::OwnedEffectsHost,
    ipc::NativeRequestOwner,
    runtime::{CancelToken, Services},
};

/// Called on core. Preparation, contention waits and unpublished-temp cleanup
/// all run on the worker, never while native admission or the store is held.
pub(crate) fn project_file_renamed_owned(
    services: Services,
    origin: NativeRequestOwner,
    destination: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    effects: OwnedEffectsHost,
    old: String,
    new: String,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    project_file_renamed_owned_extra(
        services,
        origin,
        destination,
        participants,
        cancel,
        pending,
        Vec::new(),
        effects,
        old,
        new,
        done,
    );
}

pub(crate) fn project_file_renamed_owned_extra(
    services: Services,
    origin: NativeRequestOwner,
    destination: NativeRequestOwner,
    mut participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
    old: String,
    new: String,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    participants.push(destination);
    let admission = std::sync::Arc::new(move |effect: &mut dyn FnMut() -> Result<(), String>| {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_owned_effect_many_extra(
            &origin,
            &participants,
            &cancel,
            &pending,
            &extra,
            effect,
        )
    });
    project_file_renamed(services, old, new, admission, done);
}

pub(crate) fn project_file_renamed_recovery(
    services: Services,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    receiver: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    effects: OwnedEffectsHost,
    old: String,
    new: String,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let admission = std::sync::Arc::new(move |effect: &mut dyn FnMut() -> Result<(), String>| {
        effects.with_recovery_open_effect_many(
            &receipt,
            &receiver,
            &participants,
            &cancel,
            &pending,
            effect,
        )
    });
    project_file_renamed(services, old, new, admission, done);
}
pub(crate) fn project_file_renamed_existing_document(
    services: Services,
    destination: shell_contract::ipc::NativeSaveDestination,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
    old: String,
    new: String,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let admission = std::sync::Arc::new(move |effect: &mut dyn FnMut() -> Result<(), String>| {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_existing_document_destination_effect(
            &destination,
            destination.path(),
            &participants,
            &cancel,
            &pending,
            &extra,
            effect,
        )
    });
    project_file_renamed(services, old, new, admission, done);
}

type Admission = std::sync::Arc<
    dyn Fn(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String> + Send + Sync,
>;

fn project_file_renamed(
    services: Services,
    old: String,
    new: String,
    admission: Admission,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let done = super::recovery_saved::WorkerDone::new(shell_ipc_w::home_files::back_to_core(
        services.core.clone(),
        done,
    ));
    let store = crate::host::w_projects::store().clone();
    services.workers.spawn(Box::new(move || {
        let run = || -> Result<(), String> {
            'prepare: loop {
                admission(&mut || Ok(()))?;
                let context = store
                    .lock()
                    .map_err(|error| error.to_string())?
                    .prepare_rename_context();
                let mut admit = |effect: &mut dyn FnMut()| {
                    admission(&mut || {
                        effect();
                        Ok(())
                    })
                };
                let mut plan = context.prepare(&old, &new, &mut admit);
                loop {
                    enum Attempt {
                        Busy,
                        Commit(RenameCommit),
                    }
                    let mut attempt = None;
                    admission(&mut || match store.try_lock() {
                        Ok(mut store) => {
                            let mut committed = |effect: &mut dyn FnMut()| {
                                effect();
                                Ok(())
                            };
                            attempt =
                                Some(Attempt::Commit(plan.commit(&mut store, &mut committed)));
                            Ok(())
                        }
                        Err(std::sync::TryLockError::WouldBlock) => {
                            attempt = Some(Attempt::Busy);
                            Ok(())
                        }
                        Err(std::sync::TryLockError::Poisoned(error)) => Err(error.to_string()),
                    })?;
                    match attempt.ok_or("project rename admission did not execute")? {
                        Attempt::Busy => drop(store.lock().map_err(|error| error.to_string())?),
                        Attempt::Commit(RenameCommit::Retry) => continue 'prepare,
                        Attempt::Commit(RenameCommit::Applied(result)) => return result,
                    }
                }
            }
        };
        done.finish(run());
    }));
}
