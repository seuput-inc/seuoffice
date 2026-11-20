use super::*;

#[derive(Clone)]
pub(crate) struct NativeRenameScope {
    pub(crate) snapshot: shell_contract::ipc::NativeSaveRevisionSnapshot,
    pub(crate) expected_path: PathBuf,
    pub(crate) destination: NativeRequestOwner,
    pub(crate) participants: Vec<NativeRequestOwner>,
    pub(crate) cancel: CancelToken,
    pub(crate) pending: CancelToken,
    pub(crate) additional: Vec<CancelToken>,
    pub(crate) effects: OwnedEffectsHost,
}

impl NativeRenameScope {
    pub(crate) fn admit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let extra = self.additional.iter().collect::<Vec<_>>();
        self.effects.with_native_save_revision_snapshot_effect(
            &self.snapshot,
            &self.participants,
            &self.cancel,
            &self.pending,
            &extra,
            &mut || {
                if self.snapshot.selection().path() != self.expected_path.as_path() {
                    return Err("the native save selection path changed".into());
                }
                effect()
            },
        )
    }
    fn effect<T>(&self, effect: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let mut effect = Some(effect);
        let mut output = None;
        self.admit(&mut || {
            output = Some(effect.take().ok_or("native rename effect repeated")?()?);
            Ok(())
        })?;
        output.ok_or_else(|| "native rename effect was not admitted".into())
    }
    fn admit_file(
        &self,
        effect: &mut dyn FnMut() -> Result<(), main_core::node_fs::NodeError>,
    ) -> Result<(), main_core::node_fs::NodeError> {
        let mut result = None;
        self.admit(&mut || {
            result = Some(effect());
            Ok(())
        })
        .map_err(|message| main_core::node_fs::NodeError {
            code: None,
            message,
        })?;
        result.unwrap_or_else(|| {
            Err(main_core::node_fs::NodeError {
                code: None,
                message: "native rename effect was not admitted".into(),
            })
        })
    }
    pub(crate) fn emit(&self, channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        if self.snapshot.selection().path() != self.expected_path.as_path() {
            return Err("the native save selection path changed".into());
        }
        let extra = self.additional.iter().collect::<Vec<_>>();
        self.effects.emit_native_save_revision_snapshot(
            &self.snapshot,
            &self.participants,
            0,
            &self.cancel,
            &self.pending,
            &extra,
            channel,
            args,
        )
    }
    fn wait_tokens(&self) -> Result<Vec<CancelToken>, String> {
        let binding = self.snapshot.selection().binding();
        let mut tokens = vec![
            self.cancel.clone(),
            self.pending.clone(),
            self.snapshot.loss_token(),
            binding.operation().cancel().clone(),
            binding.operation().shutdown().clone(),
            binding.operation().loss_token(),
        ];
        tokens.extend(self.additional.iter().cloned());
        for owner in std::iter::once(binding.owner())
            .chain(self.snapshot.aliases())
            .chain(self.participants.iter())
        {
            tokens.push(
                owner
                    .loss_token()
                    .ok_or("a native rename owner lifetime is unavailable")?,
            );
        }
        Ok(tokens)
    }
}

impl TextDocs {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn file_renamed_existing_document(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: &Path,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        if destination.context.document_lease().is_none() {
            return then(Err(kit::error(
                "the destination document owner was not captured before its move",
            )));
        }
        let destination = match self.capture_document_owner(view, destination.clone()) {
            Ok(owner) => owner,
            Err(error) => return then(Err(error)),
        };
        let snapshot = match authority.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return then(Err(kit::error(error))),
        };
        if let Some(active) = self.owned_saves.borrow().get(&view) {
            let same_binding = matches!(active.authority(),SaveAuthority::Native(binding) if snapshot.selection().matches_binding(binding));
            if !same_binding
                || !active.cancel().same_token(cancel)
                || !active.pending().same_token(pending)
                || !Arc::ptr_eq(
                    &active.destination().context.as_arc(),
                    &destination.context.as_arc(),
                )
            {
                return then(Err(kit::error(
                    "document rename conflicts with its pending save",
                )));
            }
        }
        let Some(effects) = self.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return then(Err(kit::error("native rename effects are unavailable")));
        };
        let participants = std::iter::once(destination.clone())
            .chain(participants.iter().cloned())
            .collect();
        let scope = NativeRenameScope {
            snapshot,
            expected_path: expected_path.to_path_buf(),
            destination,
            participants,
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
            effects,
        };
        let old = old_path.to_string_lossy().into_owned();
        let new = new_path.to_string_lossy().into_owned();
        let before = match scope.effect(|| {
            let views = self.views.borrow();
            let state = views.get(&view).ok_or("the document is no longer open")?;
            if state.save.as_deref() != Some(old.as_str()) {
                return Err("the saved document changed before its move".into());
            }
            Ok((state.tracked, state.open.clone(), state.save.clone()))
        }) {
            Ok(value) => value,
            Err(error) => return then(Err(kit::error(error))),
        };
        let next_open = if before.1.as_deref() == Some(old.as_str()) {
            Some(new.clone())
        } else {
            before.1.clone()
        };
        let next_save = Some(new.clone());
        let tracked = before.0;
        let prepare = self.hooks.native_save_asset_paths.clone();
        let target = scope.destination.clone();
        let worker_open = next_open.clone();
        let worker_save = next_save.clone();
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prepare(
                        &target,
                        tracked,
                        worker_open.as_deref(),
                        worker_save.as_deref(),
                    )
                }))
                .unwrap_or_else(|_| Err("native rename asset preparation panicked".into()))
            },
            move |prepared| {
                let grant = match prepared {
                    Ok(grant) => grant,
                    Err(error) => return then(Err(kit::error(error))),
                };
                docs.commit_native_rename(
                    view, old, new, before, next_open, next_save, scope, grant, then,
                );
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_native_rename(
        self: &Rc<Self>,
        view: ViewId,
        old: String,
        new: String,
        before: (bool, Option<String>, Option<String>),
        next_open: Option<String>,
        next_save: Option<String>,
        scope: NativeRenameScope,
        mut grant: super::super::super::RecoveryAssetStage,
        then: Then<()>,
    ) {
        let waiter = (grant.subscribe)();
        let applied = scope.effect(|| {
            let mut views = self.views.borrow_mut();
            let state = views
                .get_mut(&view)
                .ok_or("the document is no longer open")?;
            if state.tracked != before.0 || state.open != before.1 || state.save != before.2 {
                return Err("the document changed while its native rename was prepared".into());
            }
            if !(grant.commit)()? {
                return Ok(false);
            }
            state.save = next_save.clone();
            state.open = next_open.clone();
            if state.allowed.contains(&old) {
                state.allowed.insert(new.clone());
            }
            if let Some(recorded) = state.disk.remove(&old) {
                state.disk.insert(new.clone(), recorded);
            }
            Ok(true)
        });
        match applied {
            Err(error) => return then(Err(kit::error(error))),
            Ok(false) => {
                let tokens = match scope.wait_tokens() {
                    Ok(tokens) => tokens,
                    Err(error) => return then(Err(kit::error(error))),
                };
                let docs = Rc::clone(self);
                return kit::on_worker(
                    &self.services,
                    move || {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waiter(tokens)))
                            .unwrap_or_else(|_| Err("native rename asset wait panicked".into()))
                    },
                    move |result| {
                        if let Err(error) = result {
                            return then(Err(kit::error(error)));
                        }
                        docs.commit_native_rename(
                            view, old, new, before, next_open, next_save, scope, grant, then,
                        );
                    },
                );
            }
            Ok(true) => {}
        }
        let html = self.ch.app == "html";
        let worker = scope.clone();
        let (from, to) = (old.clone(), new.clone());
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
            if html{main_core::html::asset_lifecycle::rename_owned_asset_document_authorized(&from,&to,&mut|effect|worker.admit_file(effect))}
            else{main_core::markdown::asset_lifecycle::rename_owned_asset_document_authorized(&from,&to,&mut|effect|worker.admit_file(effect))}
        })).map_err(|_|"native rename asset worker panicked".to_owned())?.map_err(|error|error.message)
            },
            move |result| {
                if let Err(error) = result {
                    return then(Err(kit::error(error)));
                }
                let recovery = docs.recovery.clone();
                let recovery_scope = scope.clone();
                let next = new.clone();
                let finished = move |result: Result<(), String>| {
                    if let Err(error) = result {
                        return then(Err(kit::error(error)));
                    }
                    then(
                        scope
                            .emit(docs.ch.file_renamed, vec![new.into_ipc()])
                            .map_err(kit::error),
                    );
                };
                recovery.renamed_native_async(
                    view,
                    &old,
                    &next,
                    recovery_scope,
                    Box::new(finished),
                );
            },
        );
    }
}
