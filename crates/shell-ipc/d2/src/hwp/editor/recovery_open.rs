use super::*;
use main_core::packages::electron_utils::recovery_contract::{
    RecoveryOpenDecision, RecoveryOpenDone,
};
use shell_contract::ipc::{
    NativeInvocationSelection, NativeRecoveryCopy, NativeRecoveryDecision, RecoveryOpenContext,
    RecoveryOpenTarget,
};
use shell_contract::runtime::RendererGeneration;

fn file_selection(
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    request: &RecoveryOpenFileRequest,
) -> bool {
    match &snapshot.context {
        RecoveryOpenContext::Renderer { .. } => true,
        RecoveryOpenContext::NativeStartup(operation) => matches!(operation.selection(),
            NativeInvocationSelection::RecoveryFile { surface: shell_contract::Surface::Hwp, path, decision }
            if path == Path::new(&request.file_path)
                && matches!((decision, request.decision),
                    (NativeRecoveryDecision::Restore, RecoveryOpenDecision::Restore)
                    | (NativeRecoveryDecision::Original, RecoveryOpenDecision::Original))),
    }
}

fn untitled_selection(
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    request: &RecoveryOpenUntitledRequest,
) -> bool {
    match &snapshot.context {
        RecoveryOpenContext::Renderer { .. } => true,
        RecoveryOpenContext::NativeStartup(operation) => match operation.selection() {
            NativeInvocationSelection::RecoveryCopy {
                surface: shell_contract::Surface::Hwp,
                copy,
                title,
                save_as_default,
            } => {
                let matches_copy = match (copy, &request.copy) {
                    (NativeRecoveryCopy::File { path }, RecoveryIdentity::File(file)) => {
                        file.kind == "hwp" && path == Path::new(&file.file_path)
                    }
                    (
                        NativeRecoveryCopy::Untitled { id, ext },
                        RecoveryIdentity::Untitled(copy),
                    ) => copy.kind == "hwp" && id == &copy.untitled_id && ext == &copy.ext,
                    _ => false,
                };
                matches_copy
                    && title == &request.title
                    && save_as_default.as_deref()
                        == request.save_as_default_path.as_deref().map(Path::new)
            }
            _ => false,
        },
    }
}

impl HwpEditors {
    pub(super) fn recover_file_seed(
        &self,
        view: ViewId,
        request: &RecoveryOpenFileRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        let snapshot = match target.authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(message) => {
                return done(Err(main_core::shell::renderer_disposal::Failure(message)));
            }
        };
        if target.view() != view || !file_selection(&snapshot, request) {
            return done(Err(main_core::shell::renderer_disposal::Failure(
                "HWP recovery file selection does not match its target".into(),
            )));
        }
        let mut retired = None;
        let receipt = Rc::new(RefCell::new(None));
        let token = kit::unique().to_string();
        let result =
            self.0
                .owned_effects
                .commit_recovery_seed_snapshot(target, &snapshot, &mut || {
                    if !self
                        .0
                        .get(view, |state| state.granted && state.recovery_open.is_none())
                    {
                        return Err("HWP recovery view is not available".into());
                    }
                    let same_path = self.0.get(view, |state| {
                        state.open_path.as_deref() == Some(request.file_path.as_str())
                    });
                    if same_path && !self.0.get(view, |state| state.document_epoch.is_some()) {
                        return Err("HWP recovery document is not tracked".into());
                    }
                    self.0
                        .recovery
                        .borrow_mut()
                        .stage_file_decision(wc(view), request);
                    self.0.change(view, |state| {
                        if state.open_path.as_deref() != Some(request.file_path.as_str()) {
                            retired = state.document_epoch.replace(RendererGeneration::new());
                            state.open_path = Some(request.file_path.clone());
                            state.allowed.insert(request.file_path.clone());
                            state.save_path = Some(request.file_path.clone());
                            state.untitled_copy = None;
                        }
                    });
                    self.0.change(view, |state| {
                        let document = state
                            .document_epoch
                            .clone()
                            .expect("document epoch validated or installed in the same admission");
                        state.recovery_open = Some(RecoveryLoad {
                            receipt: receipt.clone(),
                            document,
                            path: request.file_path.clone(),
                            token: token.clone(),
                            consumed: false,
                            read_started: false,
                            read_completed: false,
                        });
                        Ok(())
                    })?;
                    Ok(())
                });
        if let Some(retired) = retired {
            retired.revoke();
        }
        match result {
            Ok(committed) => {
                *receipt.borrow_mut() = Some(committed);
                done(Ok(()));
            }
            Err(message) => done(Err(main_core::shell::renderer_disposal::Failure(message))),
        }
    }

    pub(super) fn recover_untitled_seed(
        &self,
        view: ViewId,
        request: &RecoveryOpenUntitledRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        let snapshot = match target.authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(message) => {
                return done(Err(main_core::shell::renderer_disposal::Failure(message)));
            }
        };
        if target.view() != view
            || request.copy.kind() != "hwp"
            || !untitled_selection(&snapshot, request)
        {
            return done(Err(main_core::shell::renderer_disposal::Failure(
                "HWP recovery copy selection does not match its target".into(),
            )));
        }
        let user_data = self
            .0
            .services
            .paths
            .path(shell_contract::runtime::PathName::UserData)
            .to_string_lossy()
            .into_owned();
        let path = match main_core::packages::electron_utils::recovery_store::recovery_copy_path(
            &user_data,
            &request.copy,
        ) {
            Ok(path) => path,
            Err(error) => {
                return done(Err(main_core::shell::renderer_disposal::Failure(
                    error.message,
                )));
            }
        };
        let mut retired = None;
        let receipt = Rc::new(RefCell::new(None));
        let token = kit::unique().to_string();
        let result =
            self.0
                .owned_effects
                .commit_recovery_seed_snapshot(target, &snapshot, &mut || {
                    if !self
                        .0
                        .get(view, |state| state.granted && state.recovery_open.is_none())
                    {
                        return Err("HWP recovery view is not available".into());
                    }
                    let copy =
                        self.0
                            .recovery
                            .borrow_mut()
                            .stage_untitled_copy(wc(view), request, &path);
                    self.0.change(view, |state| {
                        retired = state.document_epoch.replace(RendererGeneration::new());
                        state.open_path = Some(path.clone());
                        state.allowed.insert(path.clone());
                        state.save_path = None;
                        state.untitled_copy = Some(copy.clone());
                    });
                    self.0.change(view, |state| {
                        let document = state
                            .document_epoch
                            .clone()
                            .expect("document epoch installed in the same admission");
                        state.recovery_open = Some(RecoveryLoad {
                            receipt: receipt.clone(),
                            document,
                            path: path.clone(),
                            token: token.clone(),
                            consumed: false,
                            read_started: false,
                            read_completed: false,
                        });
                        Ok(())
                    })?;
                    Ok(())
                });
        if let Some(retired) = retired {
            retired.revoke();
        }
        match result {
            Ok(committed) => {
                *receipt.borrow_mut() = Some(committed);
                done(Ok(()));
            }
            Err(message) => done(Err(main_core::shell::renderer_disposal::Failure(message))),
        }
    }
}

impl Hwp {
    fn recovery_receiver(
        &self,
        cx: &CallContext,
    ) -> Result<
        Option<(
            shell_contract::ipc::RecoverySeedReceipt,
            shell_contract::ipc::NativeRequestOwner,
        )>,
        IpcError,
    > {
        let view = cx.sender.view;
        let captured = self.get(view, |state| {
            state
                .recovery_open
                .as_ref()
                .map(|pending| (pending.receipt.borrow().clone(), pending.document.clone()))
        });
        let Some((receipt, document)) = captured else {
            return Ok(None);
        };
        let receipt = receipt.ok_or_else(|| error("HWP recovery seed receipt is unavailable"))?;
        let native = cx
            .native
            .clone()
            .ok_or_else(|| error("HWP recovery renderer is not authenticated"))?;
        let owner = self.capture_hwp_owner(
            view,
            shell_contract::ipc::NativeRequestOwner {
                view,
                context: native,
            },
        )?;
        if !owner
            .context
            .document_lease()
            .is_some_and(|current| current.same_generation(&document))
        {
            return Err(error("HWP recovery document was replaced"));
        }
        Ok(Some((receipt, owner)))
    }

    pub(super) fn read_recovery_file(
        self: &Rc<Self>,
        cx: &CallContext,
        path: String,
        reply: shell_contract::ipc::Responder,
    ) {
        use main_core::hwp::recovery::PreparedRecoveryRead;
        use main_core::packages::electron_utils::recovery_store::{
            RecoveryCopyState, RecoveryPinnedRead,
        };
        let view = cx.sender.view;
        let (receipt, receiver) = match self.recovery_receiver(cx) {
            Ok(Some(value)) => value,
            Ok(None) => return reply.reply(Err(error("HWP recovery seed is unavailable"))),
            Err(error) => return reply.reply(Err(error)),
        };
        let authority = match receipt.target().authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(message) => return reply.reply(Err(error(message))),
        };
        let user_data = self
            .services
            .paths
            .path(shell_contract::runtime::PathName::UserData)
            .to_string_lossy()
            .into_owned();
        let mut prepared = None;
        let mut original = None;
        let mut copy = None;
        let admission = self.owned_effects.with_recovery_open_snapshot(
            &receipt,
            &authority,
            &receiver,
            &[],
            &cx.cancel,
            &cx.cancel,
            &mut || {
                {
                    let views = self.views.borrow();
                    let pending = views
                        .get(&view)
                        .and_then(|state| state.recovery_open.as_ref())
                        .ok_or("HWP recovery seed is unavailable")?;
                    if !pending.consumed || pending.path != path || pending.read_started {
                        return Err("HWP recovery read does not match its pending seed".into());
                    }
                }
                let plan = self.with_recovery(|recovery, electron, _| {
                    recovery.prepare_recovery_read(electron, wc(view), &path)
                });
                let identity = match &plan {
                    PreparedRecoveryRead::Original => None,
                    PreparedRecoveryRead::Untitled(identity) => Some(identity),
                    PreparedRecoveryRead::File {
                        identity,
                        decision: Some(RecoveryOpenDecision::Restore),
                    } => Some(identity),
                    PreparedRecoveryRead::File { .. } => {
                        return Err("HWP recovery selection is no longer available".into());
                    }
                };
                original = Some(std::fs::File::open(&path).map_err(|error| {
                    main_core::node_fs::node_error(&error, "open", &path).message
                })?);
                if let Some(identity) = identity {
                    match RecoveryPinnedRead::open(&user_data, identity) {
                        Ok(pinned) => copy = Some(pinned),
                        Err(_) => {}
                    }
                }
                self.change(view, |state| {
                    state
                        .recovery_open
                        .as_mut()
                        .expect("validated in admission")
                        .read_started = true
                });
                prepared = Some(plan);
                Ok(())
            },
        );
        if let Err(message) = admission {
            return reply.reply(Err(error(message)));
        }
        let (Some(plan), Some(mut original)) = (prepared, original) else {
            return reply.reply(Err(error("HWP recovery read was not admitted")));
        };
        let engine = Arc::clone(&self.engine);
        let hwp = Rc::clone(self);
        let cancel = cx.cancel.clone();
        let worker_path = path.clone();
        let error_path = path.clone();
        on_worker(
            &self.services,
            move || {
                use std::io::Read as _;
                let metadata = original.metadata().map_err(|error| {
                    main_core::node_fs::node_error(&error, "stat", &error_path).message
                })?;
                if metadata.len() > MAX_HWP_SAVE_BYTES as u64 {
                    return Err("hwp: file is too large to open".to_owned());
                }
                let mut bytes = Vec::new();
                original
                    .take(MAX_HWP_SAVE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| {
                        main_core::node_fs::node_error(&error, "open", &error_path).message
                    })?;
                if bytes.len() > MAX_HWP_SAVE_BYTES {
                    return Err("hwp: file is too large to open".to_owned());
                }
                let stat = main_core::shell::recent_files::StatInfo::of(&metadata);
                let disk = SaveDiskState {
                    mtime_ms: stat.mtime_ms,
                    size: stat.size as f64,
                    hash: main_core::markdown::fs_util::sha256_hex(&bytes),
                };
                let read =
                    read_hwp_bytes(&engine, bytes, Some(disk)).map_err(|error| error.message)?;
                let snapshot = match copy {
                    Some(copy) => match copy.read() {
                        Ok(snapshot) => Some(snapshot),
                        Err(_) => None,
                    },
                    None => None,
                };
                Ok((plan, read, snapshot))
            },
            move |result| {
                let (plan, read, snapshot) = match result {
                    Ok(result) => result,
                    Err(message) => return reply.reply(Err(error(message))),
                };
                let identity = match (&plan, &snapshot) {
                    (PreparedRecoveryRead::Untitled(identity), _)
                    | (PreparedRecoveryRead::File { identity, .. }, Some(_)) => {
                        Some(identity.clone())
                    }
                    _ => None,
                };
                if let Some(identity) = &identity {
                    let result = hwp.owned_effects.with_recovery_open_snapshot(
                        &receipt,
                        &authority,
                        &receiver,
                        &[],
                        &cancel,
                        &cancel,
                        &mut || {
                            hwp.with_recovery(|recovery, electron, shared| {
                                recovery.clear_recovery_discarded(electron, shared, identity)
                            })
                            .map_err(|error| error.message)
                        },
                    );
                    if let Err(message) = result {
                        return reply.reply(Err(error(message)));
                    }
                }
                let metadata = snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.prepare_copy_state(RecoveryCopyState::Undecided));
                let mut copy_bytes = match plan {
                    PreparedRecoveryRead::File { .. } => snapshot.map(|snapshot| snapshot.bytes),
                    _ => None,
                };
                let effects = Arc::clone(&hwp.owned_effects);
                let scoped_receipt = receipt.clone();
                let scoped_authority = authority.clone();
                let scoped_receiver = receiver.clone();
                let scoped_cancel = cancel.clone();
                let next = Rc::clone(&hwp);
                on_worker(
                    &hwp.services,
                    move || -> Result<(), String> {
                        if let Some(plan) = metadata {
                            let denied = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let flag = denied.clone();
                            let admit: main_core::packages::electron_utils::atomic_write::owned::Admission = Arc::new(move |effect| {
                        let mut effect = Some(effect);
                        let mut value = None;
                        effects.with_recovery_open_snapshot(&scoped_receipt, &scoped_authority, &scoped_receiver, &[], &scoped_cancel, &scoped_cancel, &mut || {
                            value = Some(effect.take().ok_or("HWP recovery metadata was admitted twice")?());
                            Ok(())
                        }).map_err(|message| {
                            flag.store(true, std::sync::atomic::Ordering::Release);
                            main_core::node_fs::NodeError { code: None, message }
                        })?;
                        value.ok_or_else(|| main_core::node_fs::NodeError { code: None, message: "HWP recovery metadata was not admitted".into() })
                    });
                            let result = plan.write_via(&mut |path, value| Ok(main_core::packages::electron_utils::atomic_write::owned::write_json(path, value, admit.clone()).is_ok()));
                            if denied.load(std::sync::atomic::Ordering::Acquire) {
                                return Err("HWP recovery metadata authority was revoked".into());
                            }
                            result.map_err(|error| error.message)?;
                        }
                        Ok(())
                    },
                    move |marked| {
                        if let Err(message) = marked {
                            return reply.reply(Err(error(message)));
                        }
                        let mut read = Some(read);
                        let mut output = None;
                        let admitted = next.owned_effects.with_recovery_open_snapshot(
                            &receipt,
                            &authority,
                            &receiver,
                            &[],
                            &cancel,
                            &cancel,
                            &mut || {
                                let mut views = next.views.borrow_mut();
                                let state = views
                                    .get_mut(&view)
                                    .ok_or("HWP recovery view is unavailable")?;
                                let pending = state
                                    .recovery_open
                                    .as_mut()
                                    .ok_or("HWP recovery seed is unavailable")?;
                                if !pending.consumed
                                    || !pending.read_started
                                    || pending.read_completed
                                    || pending.path != worker_path
                                    || !state.document_epoch.as_ref().is_some_and(|epoch| {
                                        epoch.same_generation(&pending.document)
                                    })
                                {
                                    return Err(
                                        "HWP recovery document changed before delivery".into()
                                    );
                                }
                                let read = read
                                    .take()
                                    .ok_or("HWP recovery input was already delivered")?;
                                if let Some(disk) = read.disk {
                                    state.disk_states.insert(worker_path.clone(), disk);
                                }
                                state.tracked = read.tracked;
                                state.tracking_dropped = read.dropped;
                                state.unverified = read.unverified.then(|| worker_path.clone());
                                state.base = read.base;
                                pending.read_completed = true;
                                output = Some(copy_bytes.take().unwrap_or(read.bytes));
                                drop(views);
                                if identity.is_some() {
                                    next.recovery.borrow_mut().finish_recovery_read(wc(view));
                                }
                                Ok(())
                            },
                        );
                        match (admitted, output) {
                            (Ok(()), Some(bytes)) => reply.reply(Ok(u8_array(bytes))),
                            (Err(message), _) => reply.reply(Err(error(message))),
                            _ => reply.reply(Err(error("HWP recovery input was not delivered"))),
                        }
                    },
                );
            },
        );
    }

    pub(super) fn consume_recovery(&self, cx: &CallContext) -> Result<IpcValue, IpcError> {
        let Some((receipt, receiver)) = self.recovery_receiver(cx)? else {
            return Ok(IpcValue::Null);
        };
        let authority = receipt.target().authority_snapshot().map_err(error)?;
        let mut value = None;
        self.owned_effects
            .with_recovery_open_snapshot(
                &receipt,
                &authority,
                &receiver,
                &[],
                &cx.cancel,
                &cx.cancel,
                &mut || {
                    let mut views = self.views.borrow_mut();
                    let pending = views
                        .get_mut(&cx.sender.view)
                        .and_then(|state| state.recovery_open.as_mut())
                        .ok_or("HWP recovery seed is unavailable")?;
                    if pending.consumed {
                        return Err("HWP recovery seed was already consumed".into());
                    }
                    pending.consumed = true;
                    value = Some(obj([
                        ("path", pending.path.as_str().into_ipc()),
                        ("token", pending.token.as_str().into_ipc()),
                    ]));
                    drop(views);
                    self.recovery
                        .borrow_mut()
                        .note_hwp_path_consumed(wc(cx.sender.view));
                    Ok(())
                },
            )
            .map_err(error)?;
        value.ok_or_else(|| error("HWP recovery seed was not admitted"))
    }

    pub(super) fn finish_recovery(
        &self,
        cx: &CallContext,
        args: &Args,
    ) -> Result<IpcValue, IpcError> {
        let Some((receipt, receiver)) = self.recovery_receiver(cx)? else {
            return Err(error("HWP recovery seed is unavailable"));
        };
        let authority = receipt.target().authority_snapshot().map_err(error)?;
        let token = args
            .str(0)
            .ok_or_else(|| error("HWP recovery token is missing"))?;
        let matches = self.get(cx.sender.view, |state| {
            state
                .recovery_open
                .as_ref()
                .is_some_and(|pending| pending.consumed && pending.token == token)
        });
        if !matches {
            return Err(error("HWP recovery token does not match"));
        }
        let result = match field(args.get(1), "ok") {
            IpcValue::Bool(true) => Ok(()),
            IpcValue::Bool(false) => Err(as_str(field(args.get(1), "error"))
                .unwrap_or("HWP recovery failed")
                .to_owned()),
            _ => return Err(error("HWP recovery result is invalid")),
        };
        if result.is_ok()
            && !self.get(cx.sender.view, |state| {
                state
                    .recovery_open
                    .as_ref()
                    .is_some_and(|pending| pending.read_completed)
            })
        {
            return Err(error("HWP recovery input has not been read"));
        }
        self.owned_effects
            .settle_recovery_seed_snapshot(&receipt, &authority, &receiver, &cx.cancel, result)
            .map_err(error)?;
        self.change(cx.sender.view, |state| {
            state.recovery_open = None;
        });
        Ok(IpcValue::Undefined)
    }
}
