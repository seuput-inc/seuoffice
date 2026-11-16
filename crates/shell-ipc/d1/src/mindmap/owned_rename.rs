use super::*;
use crate::mindmap::{app, recovery};

#[derive(Clone)]
enum RenameAuthority {
    Owned(NativeRequestOwner),
    Native(shell_contract::ipc::NativeExistingDocumentBinding),
    NativeRevision(
        shell_contract::ipc::NativeSaveRevisionSnapshot,
        std::path::PathBuf,
    ),
    Recovery {
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: NativeRequestOwner,
    },
}

#[derive(Clone)]
struct RenameScope {
    authority: RenameAuthority,
    destination: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    document: RendererGeneration,
    effects: OwnedEffectsHost,
}

impl RenameScope {
    fn effect<T>(&self, action: impl FnOnce() -> Result<T, IpcError>) -> Result<T, IpcError> {
        let mut action = Some(action);
        let mut result = None;
        let mut effect = || {
            if let RenameAuthority::NativeRevision(snapshot, expected) = &self.authority
                && snapshot.selection().path() != expected
            {
                return Err("The native save destination changed".into());
            }
            let value = action
                .take()
                .ok_or("The rename effect was already consumed")?();
            let error = value.as_ref().err().map(|error| error.message.clone());
            result = Some(value);
            error.map_or(Ok(()), Err)
        };
        let participants: Vec<_> = std::iter::once(self.destination.clone())
            .chain(self.participants.iter().cloned())
            .collect();
        let admitted = match &self.authority {
            RenameAuthority::Owned(origin) => self.effects.with_owned_effect_many_extra(
                origin,
                &participants,
                &self.cancel,
                &self.pending,
                &self.additional.iter().collect::<Vec<_>>(),
                &mut effect,
            ),
            RenameAuthority::Native(binding) => {
                self.effects.with_existing_document_save_effect_extra(
                    binding,
                    &participants,
                    &self.cancel,
                    &self.pending,
                    &self.additional.iter().collect::<Vec<_>>(),
                    &mut effect,
                )
            }
            RenameAuthority::NativeRevision(snapshot, _) => {
                self.effects.with_native_save_revision_snapshot_effect(
                    snapshot,
                    &participants,
                    &self.cancel,
                    &self.pending,
                    &self.additional.iter().collect::<Vec<_>>(),
                    &mut effect,
                )
            }
            RenameAuthority::Recovery { receipt, receiver } => {
                self.effects.with_recovery_open_effect_many(
                    receipt,
                    receiver,
                    &participants,
                    &self.cancel,
                    &self.pending,
                    &mut effect,
                )
            }
        };
        match result {
            Some(result) => result,
            None => {
                admitted.map_err(thrown)?;
                Err(thrown("The rename effect did not run"))
            }
        }
    }

    fn revision_effect<T>(
        &self,
        app: &MindmapApp,
        revision: &MapRevision,
        action: impl FnOnce() -> T,
    ) -> Result<T, IpcError> {
        self.effect(|| {
            let state = app.0.state.borrow();
            let view = self.destination.view;
            if !state
                .documents
                .get(&view)
                .is_some_and(|document| document.same_generation(&self.document))
                || state.revisions.get(&view).copied() != Some(revision.revision)
                || state.save_path.get(&view) != revision.path.as_ref()
            {
                return Err(thrown("The map changed while the operation was pending"));
            }
            drop(state);
            Ok(action())
        })
    }

    fn admission(&self, app: &MindmapApp, revision: MapRevision) -> Admission {
        let core = app.0.services.core.clone();
        let scope = self.clone();
        Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1);
            let scope = scope.clone();
            let revision = revision.clone();
            core.post(Box::new(move || {
                let result = crate::mindmap::handle()
                    .ok_or_else(|| thrown("The map application was released"))
                    .and_then(|app| scope.revision_effect(&app, &revision, effect));
                let _ = send.send(result.map_err(|error| NodeError {
                    code: None,
                    message: error.message,
                }));
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The map operation dispatcher was released".into(),
            })?
        })
    }

    fn emit(&self, channel: &str, args: Vec<IpcValue>) -> Result<(), IpcError> {
        let destinations: Vec<_> = std::iter::once(self.destination.clone())
            .chain(self.participants.iter().cloned())
            .collect();
        match &self.authority {
            RenameAuthority::Owned(origin) => self.effects.emit_owned_many_extra(
                origin,
                &destinations,
                0,
                &self.cancel,
                &self.pending,
                &self.additional.iter().collect::<Vec<_>>(),
                channel,
                args,
            ),
            RenameAuthority::Native(binding) => {
                self.effects.emit_existing_document_save_many_extra(
                    binding,
                    &destinations,
                    0,
                    &self.cancel,
                    &self.pending,
                    &self.additional.iter().collect::<Vec<_>>(),
                    channel,
                    args,
                )
            }
            RenameAuthority::NativeRevision(snapshot, expected)
                if snapshot.selection().path() == expected =>
            {
                self.effects.emit_native_save_revision_snapshot(
                    snapshot,
                    &destinations,
                    0,
                    &self.cancel,
                    &self.pending,
                    &self.additional.iter().collect::<Vec<_>>(),
                    channel,
                    args,
                )
            }
            RenameAuthority::NativeRevision(_, _) => {
                Err("The native save destination changed".into())
            }
            RenameAuthority::Recovery { receipt, receiver } => {
                self.effects.emit_recovery_open_many(
                    receipt,
                    receiver,
                    &destinations,
                    0,
                    &self.cancel,
                    &self.pending,
                    channel,
                    args,
                )
            }
        }
        .map_err(thrown)
    }
}

impl MindmapApp {
    pub(in crate::mindmap) fn capture_save_owner(
        &self,
        view: ViewId,
        mut native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError> {
        let state = self.0.state.borrow();
        let lease = state
            .documents
            .get(&view)
            .ok_or_else(|| thrown("The map document was released"))?;
        if native.view != view
            || !native
                .context
                .renderer()
                .is_some_and(|renderer| renderer.is_active())
            || native
                .context
                .document_lease()
                .is_some_and(|captured| !captured.same_generation(lease))
        {
            return Err(thrown("The map owner belongs to another document"));
        }
        if native.context.document_lease().is_none() {
            native.context = native.context.with_document_lease(lease.clone());
        }
        Ok(native)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::mindmap) fn rename_path_owned(
        &self,
        view: ViewId,
        old: &str,
        new: &str,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    ) {
        let destination = match self.capture_save_owner(view, destination.clone()) {
            Ok(owner) => owner,
            Err(error) => return then(Err(error)),
        };
        self.rename_path_many_owned_extra(
            view,
            old,
            new,
            origin,
            &destination,
            &[],
            cancel,
            pending,
            &[],
            then,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::mindmap) fn rename_path_many_owned_extra(
        &self,
        view: ViewId,
        old: &str,
        new: &str,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        if destination.view != view || destination.context.document_lease().is_none() {
            return then(Err(thrown("The captured map document is unavailable")));
        }
        self.rename_path(
            view,
            old,
            new,
            RenameAuthority::Owned(origin.clone()),
            destination.clone(),
            participants,
            cancel,
            pending,
            additional,
            then,
        );
    }

    pub(in crate::mindmap) fn rename_path_save_owned(
        &self,
        view: ViewId,
        old: &str,
        new: &str,
        request: &crate::owned_save::OwnedSave,
        origin: &NativeRequestOwner,
        then: Then<()>,
    ) {
        self.rename_path(
            view,
            old,
            new,
            RenameAuthority::Owned(origin.clone()),
            request.destination.clone(),
            &request.participants(&[]),
            &request.cancel,
            &request.pending,
            &request.additional,
            then,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::mindmap) fn rename_path_native(
        &self,
        view: ViewId,
        old: &str,
        new: &str,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        if destination.view != view {
            return then(Err(thrown("The rename belongs to another view")));
        }
        self.rename_path(
            view,
            old,
            new,
            RenameAuthority::Native(binding.clone()),
            destination.clone(),
            participants,
            cancel,
            pending,
            additional,
            then,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::mindmap) fn rename_path_native_revision(
        &self,
        view: ViewId,
        old: &str,
        new: &str,
        snapshot: &shell_contract::ipc::NativeSaveRevisionSnapshot,
        expected_path: &std::path::Path,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        if destination.view != view {
            return then(Err(thrown("The rename belongs to another view")));
        }
        self.rename_path(
            view,
            old,
            new,
            RenameAuthority::NativeRevision(snapshot.clone(), expected_path.to_path_buf()),
            destination.clone(),
            participants,
            cancel,
            pending,
            additional,
            then,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::mindmap) fn rename_path_recovery(
        &self,
        view: ViewId,
        old: &str,
        new: &str,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    ) {
        if destination.view != view {
            return then(Err(thrown("The rename belongs to another view")));
        }
        self.rename_path(
            view,
            old,
            new,
            RenameAuthority::Recovery {
                receipt: receipt.clone(),
                receiver: receiver.clone(),
            },
            destination.clone(),
            participants,
            cancel,
            pending,
            &[],
            then,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn rename_path(
        &self,
        view: ViewId,
        old: &str,
        new: &str,
        authority: RenameAuthority,
        destination: NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        let document = match destination.context.document_lease().cloned() {
            Some(lease) => lease,
            None => return then(Err(thrown("The map document lease is unavailable"))),
        };
        let effects = match self.0.services.hosts.get::<OwnedEffectsHost>().cloned() {
            Some(host) => host,
            None => return then(Err(thrown("The native save effect host is unavailable"))),
        };
        let request = RenameScope {
            authority,
            destination,
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
            document,
            effects,
        };
        let changed = request.effect(|| {
            let mut state = self.0.state.borrow_mut();
            if !state
                .documents
                .get(&view)
                .is_some_and(|lease| lease.same_generation(&request.document))
            {
                return Err(thrown("The map document was replaced"));
            }
            if state.save_path.get(&view).is_some_and(|path| path == old) {
                state.save_path.insert(view, new.to_owned());
            }
            if state.open_path.get(&view).is_some_and(|path| path == old) {
                state.open_path.insert(view, new.to_owned());
            }
            if let Some(allowed) = state.allowed.get_mut(&view)
                && allowed.contains(old)
            {
                allowed.insert(new.to_owned());
            }
            if let Some(states) = state.disk_states.get_mut(&view)
                && let Some(recorded) = states.remove(old)
            {
                states.insert(new.to_owned(), recorded);
            }
            for save in state.owned_saves.values_mut().filter(|save| {
                save.request.destination.view == view && save.request.pending.same_token(pending)
            }) {
                if save.baseline.as_deref() == Some(old) {
                    save.baseline = Some(new.to_owned());
                }
                if save.override_path.as_deref() == Some(old) {
                    save.override_path = Some(new.to_owned());
                }
            }
            let revision = state
                .revisions
                .get(&view)
                .copied()
                .ok_or_else(|| thrown("The map revision was released"))?;
            Ok((
                state
                    .recovery
                    .get(&view)
                    .map(|recovery| recovery.owned.clone())
                    .unwrap_or_default(),
                MapRevision {
                    revision,
                    path: state.save_path.get(&view).cloned(),
                },
            ))
        });
        let (owned, revision) = match changed {
            Ok(changed) => changed,
            Err(error) => return then(Err(error)),
        };
        let id = self.new_gen();
        self.0.state.borrow_mut().owned_renames.insert(id, then);
        let previous = old.to_owned();
        let target = new.to_owned();
        let target_for_reply = target.clone();
        let shared_recovery = self.0.recovery_state.clone();
        let directory = self.user_data_dir();
        let admission = request.admission(self, revision.clone());
        self.offload(
            move || {
                migrate_recovery(
                    &shared_recovery,
                    &directory,
                    &previous,
                    &target,
                    &owned,
                    &admission,
                )
            },
            move |app, outcome| {
                let result = outcome
                    .and_then(|migrated| {
                        request.revision_effect(app, &revision, || {
                            let mut state = app.0.state.borrow_mut();
                            if let Some((old_key, target, deleted)) = migrated
                                && let Some(recovery) = state.recovery.get_mut(&view)
                            {
                                if let Some((key, identity)) = target {
                                    match recovery
                                        .owned
                                        .iter_mut()
                                        .find(|(candidate, _)| candidate == &key)
                                    {
                                        Some(entry) => entry.1 = identity,
                                        None => recovery.owned.push((key, identity)),
                                    }
                                }
                                if deleted {
                                    recovery.owned.retain(|(key, _)| key != &old_key);
                                }
                            }
                            Ok(())
                        })
                    })
                    .and_then(|result| result)
                    .and_then(|()| {
                        request.emit("mindmap:file-renamed", vec![string(target_for_reply)])
                    });
                let then = app.0.state.borrow_mut().owned_renames.remove(&id);
                if let Some(then) = then {
                    then(result);
                }
            },
        );
    }
}

type Migration = Option<(
    String,
    Option<(
        String,
        main_core::packages::electron_utils::recovery_store::RecoveryIdentity,
    )>,
    bool,
)>;

fn migrate_recovery(
    shared: &crate::host::RecoveryStateHost,
    directory: &str,
    old: &str,
    new: &str,
    owned: &[(
        String,
        main_core::packages::electron_utils::recovery_store::RecoveryIdentity,
    )],
    admission: &Admission,
) -> Result<Migration, IpcError> {
    use main_core::packages::electron_utils::atomic_write::owned::{
        admitted, write_file, write_json,
    };
    use main_core::packages::electron_utils::recovery_store::RecoveryWritePlan;
    use std::sync::TryLockError;
    enum Prepared {
        Busy,
        Unowned,
        Ready(String, String, RecoveryWritePlan),
    }
    let prepared = loop {
        let shared = shared.clone();
        let directory = directory.to_owned();
        let from = recovery::file_identity(old);
        let to = recovery::file_identity(new);
        let owned = owned.to_vec();
        let prepared = admitted(admission, move || {
            let state = match shared.try_lock() {
                Ok(state) => state,
                Err(TryLockError::WouldBlock) => return Ok(Prepared::Busy),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(NodeError {
                        code: None,
                        message: "The recovery state is unavailable".into(),
                    });
                }
            };
            let root = || directory.clone();
            let store = create_recovery_store(
                RecoveryStoreOptions {
                    user_data_dir: &root,
                    now_ms: &app::now_ms,
                    random_uuid: &jscompat::random_uuid,
                },
                &state,
            );
            let from_key = store.copy_path(&from)?;
            if !owned.iter().any(|(key, _)| key == &from_key) {
                return Ok(Prepared::Unowned);
            }
            let to_key = store.copy_path(&to)?;
            let plan = store
                .prepare_write(&to, &Default::default())?
                .ok_or_else(|| NodeError {
                    code: None,
                    message: "The recovery migration was superseded".into(),
                })?;
            Ok::<_, NodeError>(Prepared::Ready(from_key, to_key, plan))
        })
        .map_err(|error| thrown(error.message))?
        .map_err(|error| thrown(error.message))?;
        match prepared {
            Prepared::Busy => std::thread::sleep(std::time::Duration::from_millis(5)),
            prepared => break prepared,
        }
    };
    let Prepared::Ready(from_key, to_key, plan) = prepared else {
        return Ok(None);
    };
    let mut migrated = None;
    if let Ok(bytes) = std::fs::read(&from_key) {
        let checked = recovery_ticket_admission(
            admission.clone(),
            shared.clone(),
            directory.to_owned(),
            plan.ticket().clone(),
        );
        plan.write_via(
            &bytes,
            &mut |path, metadata| write_json(path, metadata, checked.clone()),
            &mut |path, bytes| write_file(path, bytes, checked.clone()),
        )
        .map_err(|error| thrown(error.message))?;
        migrated = Some((to_key, recovery::file_identity(new)));
    }
    let deleted = crate::owned_recovery::delete_copies(
        shared,
        directory,
        &[recovery::file_identity(old)],
        RecoveryDeleteReason::Discarded,
        admission,
    )
    .map_err(|error| thrown(error.message))?;
    Ok(Some((
        from_key,
        migrated,
        deleted.first().is_some_and(|copy| !copy.refused),
    )))
}

fn recovery_ticket_admission(
    base: Admission,
    shared: crate::host::RecoveryStateHost,
    directory: String,
    ticket: main_core::packages::electron_utils::recovery_store::RecoveryWriteTicket,
) -> Admission {
    use main_core::packages::electron_utils::atomic_write::owned::{
        EffectValue, ShortEffect, admitted,
    };
    use std::sync::TryLockError;
    Arc::new(move |effect| {
        let mut effect = effect;
        loop {
            let shared = shared.clone();
            let directory = directory.clone();
            let ticket = ticket.clone();
            let result: Result<Result<EffectValue, ShortEffect>, NodeError> =
                admitted(&base, move || {
                    let state = match shared.try_lock() {
                        Ok(state) => state,
                        Err(TryLockError::WouldBlock) => return Ok(Err(effect)),
                        Err(TryLockError::Poisoned(_)) => {
                            return Err(NodeError {
                                code: None,
                                message: "The recovery state is unavailable".into(),
                            });
                        }
                    };
                    let root = || directory.clone();
                    let store = create_recovery_store(
                        RecoveryStoreOptions {
                            user_data_dir: &root,
                            now_ms: &app::now_ms,
                            random_uuid: &jscompat::random_uuid,
                        },
                        &state,
                    );
                    if !store.write_ticket_is_current(&ticket) {
                        return Err(NodeError {
                            code: None,
                            message: "The recovery migration was superseded".into(),
                        });
                    }
                    Ok(Ok(effect()))
                })?;
            match result? {
                Ok(result) => return Ok(result),
                Err(pending) => {
                    effect = pending;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    })
}
