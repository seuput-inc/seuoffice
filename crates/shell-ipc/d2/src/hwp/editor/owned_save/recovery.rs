use super::*;
use main_core::packages::electron_utils::recovery_store::{
    RecoveryDeleteReason, RecoveryFileIdentity, RecoveryStore, RecoveryStoreOptions,
    RecoveryWriteOptions, create_recovery_store,
};

#[derive(Clone)]
struct Store {
    directory: String,
    shell: HwpShellHost,
    state: Arc<Mutex<SharedRecoveryState>>,
}
impl Store {
    fn of(hwp: &Hwp) -> Self {
        Self {
            directory: hwp
                .services
                .paths
                .path(shell_contract::runtime::PathName::UserData)
                .to_string_lossy()
                .into_owned(),
            shell: hwp.shell.clone(),
            state: hwp.recovery_state.clone(),
        }
    }
    fn try_with<R>(&self, action: impl FnOnce(&RecoveryStore<'_>) -> R) -> Option<R> {
        let shared = match self.state.try_lock() {
            Ok(shared) => shared,
            Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let directory = || self.directory.clone();
        let now = || self.shell.now_ms();
        let uuid = || self.shell.random_uuid();
        Some(action(&create_recovery_store(
            RecoveryStoreOptions {
                user_data_dir: &directory,
                now_ms: &now,
                random_uuid: &uuid,
            },
            &shared,
        )))
    }
    fn wait(&self) {
        drop(
            self.state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
    }

    fn admitted<R>(
        &self,
        scope: &Scope,
        mut action: impl FnMut(&RecoveryStore<'_>) -> Result<R, String>,
    ) -> Result<R, String> {
        loop {
            let mut answer = None;
            scope.effect(&mut || {
                answer = self.try_with(&mut action);
                Ok(())
            })?;
            if let Some(answer) = answer {
                return answer;
            }
            self.wait();
        }
    }

    fn admitted_rename<R>(
        &self,
        scope: &RenameScope,
        mut action: impl FnMut(&RecoveryStore<'_>) -> Result<R, String>,
    ) -> Result<R, String> {
        loop {
            let mut answer = None;
            scope.effect(&mut || {
                answer = self.try_with(&mut action);
                Ok(())
            })?;
            if let Some(answer) = answer {
                return answer;
            }
            self.wait();
        }
    }
}
fn identity(path: &str) -> RecoveryIdentity {
    RecoveryIdentity::File(RecoveryFileIdentity {
        kind: "hwp".into(),
        file_path: path.to_owned(),
    })
}

impl Hwp {
    pub(super) fn saved_recovery_owned(
        self: &Rc<Self>,
        view: ViewId,
        before: Option<RecoveryIdentity>,
        scope: Scope,
        done: Then<()>,
    ) {
        self.cleanup_recovery_owned(view, before, RecoveryDeleteReason::Saved, scope, done);
    }

    pub(super) fn discard_recovery_owned(
        self: &Rc<Self>,
        view: ViewId,
        scope: Scope,
        done: Then<()>,
    ) {
        let mut before = None;
        if let Err(message) = scope.effect(&mut || {
            before = self.recovery.borrow().existing_identity(wc(view));
            Ok(())
        }) {
            return done(Err(error(message)));
        }
        if before.is_some() {
            return self.cleanup_recovery_owned(
                view,
                before,
                RecoveryDeleteReason::DontSave,
                scope,
                done,
            );
        }
        let paths = self.services.paths.clone();
        let hwp = self.clone();
        on_worker(
            &self.services,
            move || match read_hwp_default_format(&AppPaths(paths)) {
                HwpDefaultFormat::Hwp => HwpFormat::Hwp,
                HwpDefaultFormat::Hwpx => HwpFormat::Hwpx,
            },
            move |format| {
                let accepted = scope.effect(&mut || {
                    before = hwp.recovery.borrow_mut().hwp_recovery_identity_with_format(
                        &Electron(&hwp),
                        wc(view),
                        format,
                    );
                    Ok(())
                });
                if let Err(message) = accepted {
                    return done(Err(error(message)));
                }
                hwp.cleanup_recovery_owned(
                    view,
                    before,
                    RecoveryDeleteReason::DontSave,
                    scope,
                    done,
                );
            },
        );
    }

    fn cleanup_recovery_owned(
        self: &Rc<Self>,
        view: ViewId,
        before: Option<RecoveryIdentity>,
        reason: RecoveryDeleteReason,
        scope: Scope,
        done: Then<()>,
    ) {
        let store = Store::of(self);
        let mut resets = Vec::new();
        if let Some(before) = &before {
            let targets =
                match self
                    .recovery
                    .borrow()
                    .saved_reset_targets(&Electron(self), wc(view), before)
                {
                    Ok(targets) => targets,
                    Err(failure) => return done(Err(error(failure.message))),
                };
            for id in targets {
                let target = view_of(id);
                if let Some(generation) = self.services.views.renderer_generation(target) {
                    let owner = match self
                        .owned_effects
                        .capture_owner(target, &generation)
                        .and_then(|owner| {
                            self.capture_hwp_owner(target, owner)
                                .map_err(|error| error.message)
                        }) {
                        Ok(owner) => owner,
                        Err(_) => continue,
                    };
                    resets.push(owner);
                }
            }
        }
        self.prepare_saved_recovery(view, before, reason, scope, store, resets, done);
    }

    fn prepare_saved_recovery(
        self: &Rc<Self>,
        view: ViewId,
        before: Option<RecoveryIdentity>,
        reason: RecoveryDeleteReason,
        scope: Scope,
        store: Store,
        resets: Vec<NativeRequestOwner>,
        done: Then<()>,
    ) {
        let mut plan = None;
        let mut busy = false;
        let prepared = scope.effect(&mut || {
            if let Some(before) = &before {
                match store.try_with(|store| store.prepare_delete(before, reason)) {
                    Some(result) => plan = result.map_err(|error| error.message)?,
                    None => {
                        busy = true;
                        return Ok(());
                    }
                }
            }
            if reason == RecoveryDeleteReason::Saved {
                self.recovery.borrow_mut().begin_saved(wc(view));
            } else {
                self.recovery.borrow_mut().begin_dont_save(wc(view));
            }
            Ok(())
        });
        if let Err(message) = prepared {
            return done(Err(error(message)));
        }
        if busy {
            let waiting = store.clone();
            let hwp = self.clone();
            return on_worker(
                &self.services,
                move || waiting.wait(),
                move |()| {
                    hwp.prepare_saved_recovery(view, before, reason, scope, store, resets, done)
                },
            );
        }
        let worker_scope = scope.clone();
        let hwp = self.clone();
        on_worker(
            &self.services,
            move || match plan {
                Some(plan) => plan
                    .remove_authorized(&mut |effect| worker_scope.admit(effect))
                    .map(|()| true)
                    .map_err(|error| error.message),
                None => Ok(false),
            },
            move |result| {
                let deleted = match result {
                    Ok(deleted) => deleted,
                    Err(message) => return done(Err(error(message))),
                };
                let accepted = scope.effect(&mut || {
                    if reason == RecoveryDeleteReason::Saved {
                        hwp.recovery.borrow_mut().finish_saved(wc(view));
                    } else {
                        hwp.recovery
                            .borrow_mut()
                            .note_dont_save(&Electron(&hwp), wc(view));
                    }
                    Ok(())
                });
                if let Err(message) = accepted {
                    return done(Err(error(message)));
                }
                if deleted {
                    for recipient in resets {
                        if let Err(message) = scope.effects.emit_owned(
                            &scope.origin,
                            &recipient,
                            &scope.cancel,
                            &scope.pending,
                            "hwp:recovery-reset",
                            vec![],
                        ) {
                            if !scope
                                .origin
                                .loss_token()
                                .is_some_and(|loss| !loss.is_cancelled())
                                || scope.cancel.is_cancelled()
                                || scope.pending.is_cancelled()
                            {
                                return done(Err(error(message)));
                            }
                        }
                    }
                }
                done(Ok(()));
            },
        );
    }

    pub(super) fn renamed_recovery_owned(
        self: &Rc<Self>,
        view: ViewId,
        old: String,
        new: String,
        scope: RenameScope,
        done: Then<()>,
    ) {
        let store = Store::of(self);
        let mut follows = false;
        let mut source = None;
        let prepared = scope.effect(&mut || {
            follows = self
                .recovery
                .borrow_mut()
                .note_file_renamed(wc(view), &old, &new);
            if follows {
                source = Some(
                    main_core::packages::electron_utils::recovery_store::recovery_copy_path(
                        &store.directory,
                        &identity(&old),
                    )
                    .map_err(|error| error.message)?,
                );
            }
            Ok(())
        });
        if let Err(message) = prepared {
            return done(Err(error(message)));
        }
        if !follows {
            return done(Ok(()));
        }
        let worker_scope = scope.clone();
        on_worker(
            &self.services,
            move || -> Result<(), String> {
                if let Some(bytes) = source.and_then(|path| std::fs::read(path).ok()) {
                    let write = store
                        .admitted_rename(&worker_scope, |store| {
                            store
                                .prepare_write(
                                    &identity(&new),
                                    &RecoveryWriteOptions {
                                        title: Some(basename(&new, None)),
                                        ..Default::default()
                                    },
                                )
                                .map_err(|error| error.message)
                        })?
                        .ok_or("hwp: recovery copy rename was superseded")?;
                    write
                        .write_authorized(&bytes, &mut |effect| {
                            store
                                .admitted_rename(&worker_scope, |store| {
                                    if !store.write_ticket_is_current(write.ticket()) {
                                        return Err(
                                            "hwp: recovery copy rename was superseded".into()
                                        );
                                    }
                                    effect();
                                    Ok(())
                                })
                                .map_err(|message| main_core::node_fs::NodeError {
                                    code: None,
                                    message,
                                })
                        })
                        .map_err(|error| error.message)?;
                }
                let deletion = store.admitted_rename(&worker_scope, |store| {
                    store
                        .prepare_delete(&identity(&old), RecoveryDeleteReason::Discarded)
                        .map_err(|error| error.message)
                })?;
                if let Some(deletion) = deletion {
                    deletion
                        .remove_authorized(&mut |effect| worker_scope.admit(effect))
                        .map_err(|error| error.message)?;
                }
                Ok(())
            },
            move |result| {
                if let Err(message) = result {
                    return done(Err(error(message)));
                }
                let emitted = match &scope {
                    RenameScope::Renderer(scope) => match &scope.recovery {
                        Some((receipt, snapshot)) => scope.effects.emit_recovery_open_snapshot(
                            receipt,
                            snapshot,
                            &scope.origin,
                            &scope.participants,
                            0,
                            &scope.cancel,
                            &scope.pending,
                            "hwp:recovery-reset",
                            vec![],
                        ),
                        None => scope.effects.emit_owned_many_extra(
                            &scope.origin,
                            &scope.participants,
                            0,
                            &scope.cancel,
                            &scope.pending,
                            &scope.additional.iter().collect::<Vec<_>>(),
                            "hwp:recovery-reset",
                            vec![],
                        ),
                    },
                    RenameScope::Native {
                        snapshot,
                        expected_path,
                        participants,
                        cancel,
                        pending,
                        additional,
                        effects,
                    } => {
                        if snapshot.selection().path() != expected_path {
                            Err("hwp: native save destination changed".into())
                        } else {
                            effects.emit_native_save_revision_snapshot(
                                snapshot,
                                participants,
                                0,
                                cancel,
                                pending,
                                &additional.iter().collect::<Vec<_>>(),
                                "hwp:recovery-reset",
                                vec![],
                            )
                        }
                    }
                };
                done(emitted.map_err(error));
            },
        );
    }
}
