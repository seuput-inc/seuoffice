use super::*;
use crate::text_recovery::RecoverySeedSpec;
use main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone;
use main_core::packages::electron_utils::recovery_contract::{
    RecoveryOpenDecision, RecoveryOpenFileRequest, RecoveryOpenUntitledRequest,
};
use main_core::packages::electron_utils::recovery_store::{
    RecoveryCopyState, RecoveryFileIdentity, RecoveryPinnedRead,
};
use shell_contract::ipc::{
    NativeInvocationSelection, NativeRecoveryCopy, NativeRecoveryDecision, RecoveryOpenContext,
    RecoveryOpenTarget,
};
use shell_contract::runtime::CancelToken;

enum Selection {
    File(RecoveryOpenFileRequest),
    Untitled(RecoveryOpenUntitledRequest),
}

fn selection_matches(app: &str, target: &RecoveryOpenTarget, selection: &Selection) -> bool {
    let surface = if app == "html" {
        shell_contract::Surface::Html
    } else {
        shell_contract::Surface::Markdown
    };
    match (target.context(), selection) {
        (RecoveryOpenContext::Renderer { .. }, Selection::File(_)) => true,
        (RecoveryOpenContext::Renderer { .. }, Selection::Untitled(request)) => {
            request.copy.kind() == app
        }
        (RecoveryOpenContext::NativeStartup(operation), Selection::File(request)) => {
            matches!(operation.selection(),
            NativeInvocationSelection::RecoveryFile{surface:actual,path,decision}
                if *actual==surface && path==Path::new(&request.file_path)
                && matches!((decision,request.decision),(NativeRecoveryDecision::Restore,RecoveryOpenDecision::Restore)|(NativeRecoveryDecision::Original,RecoveryOpenDecision::Original)))
        }
        (RecoveryOpenContext::NativeStartup(operation), Selection::Untitled(request)) => {
            match operation.selection() {
                NativeInvocationSelection::RecoveryCopy {
                    surface: actual,
                    copy,
                    title,
                    save_as_default,
                } if *actual == surface => {
                    let same = match (copy, &request.copy) {
                        (NativeRecoveryCopy::File { path }, RecoveryIdentity::File(file)) => {
                            file.kind == app && path == Path::new(&file.file_path)
                        }
                        (
                            NativeRecoveryCopy::Untitled { id, ext },
                            RecoveryIdentity::Untitled(copy),
                        ) => copy.kind == app && id == &copy.untitled_id && ext == &copy.ext,
                        _ => false,
                    };
                    same && title == &request.title
                        && save_as_default.as_deref()
                            == request.save_as_default_path.as_deref().map(Path::new)
                }
                _ => false,
            }
        }
    }
}

fn read_recovery(
    target: RecoveryOpenTarget,
    effects: shell_contract::hosts::OwnedEffectsHost,
    user_data: String,
    identity: RecoveryIdentity,
    file_path: Option<String>,
) -> Result<(String, Option<SaveDiskState>), String> {
    let mut pinned = None;
    effects.with_recovery_seed_effect(&target, &mut || {
        pinned =
            Some(RecoveryPinnedRead::open(&user_data, &identity).map_err(|error| error.message)?);
        Ok(())
    })?;
    let snapshot = pinned
        .ok_or("the recovery copy was not admitted")?
        .read()
        .map_err(|error| error.message)?;
    if let Some(plan) = snapshot.prepare_copy_state(RecoveryCopyState::Undecided) {
        let denied = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = denied.clone();
        let guard = effects.clone();
        let scope = target.clone();
        let admission: main_core::packages::electron_utils::atomic_write::owned::Admission =
            Arc::new(move |effect| {
                let mut effect = Some(effect);
                let mut value = None;
                guard
                    .with_recovery_seed_effect(&scope, &mut || {
                        value = Some(effect.take().ok_or("recovery mark was admitted twice")?());
                        Ok(())
                    })
                    .map_err(|message| {
                        flag.store(true, std::sync::atomic::Ordering::Release);
                        main_core::node_fs::NodeError {
                            code: None,
                            message,
                        }
                    })?;
                value.ok_or_else(|| main_core::node_fs::NodeError {
                    code: None,
                    message: "recovery mark was not admitted".into(),
                })
            });
        let result = plan.write_via(&mut |path, metadata| {
            main_core::packages::electron_utils::atomic_write::owned::write_json(
                path,
                metadata,
                admission.clone(),
            )
            .map(|()| true)
        });
        if denied.load(std::sync::atomic::Ordering::Acquire) {
            return Err(result.err().map_or_else(
                || "recovery mark authority was revoked".into(),
                |error| error.message,
            ));
        }
        if let Err(error) = result {
            eprintln!("[recovery] mark loaded failed: {error:?}");
        }
    }
    let disk = file_path.as_deref().and_then(record_disk_state);
    Ok((String::from_utf8_lossy(&snapshot.bytes).into_owned(), disk))
}

pub(super) struct PreparedSeed {
    pub spec: Option<RecoverySeedSpec>,
    pub view: ViewId,
    pub expected: RendererGeneration,
    pub file_path: Option<String>,
    pub disk: Option<SaveDiskState>,
    pub grant: Box<dyn FnMut() -> Result<(), String> + Send>,
}

impl TextDocs {
    pub(crate) fn open_recovery_file(
        self: &Rc<Self>,
        view: ViewId,
        request: &RecoveryOpenFileRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        self.open_recovery(view, Selection::File(request.clone()), target.clone(), done)
    }
    pub(crate) fn open_recovery_untitled(
        self: &Rc<Self>,
        view: ViewId,
        request: &RecoveryOpenUntitledRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        self.open_recovery(
            view,
            Selection::Untitled(request.clone()),
            target.clone(),
            done,
        )
    }
    fn open_recovery(
        self: &Rc<Self>,
        view: ViewId,
        selection: Selection,
        target: RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        if target.view() != view || !selection_matches(self.ch.app, &target, &selection) {
            return done(Err(main_core::shell::renderer_disposal::Failure(
                "the recovery selection does not match its target".into(),
            )));
        }
        if !self
            .services
            .views
            .renderer_generation(view)
            .is_some_and(|current| current.same_generation(target.created()) && current.is_active())
        {
            return done(Err(main_core::shell::renderer_disposal::Failure(
                "the recovery target was replaced before initialization".into(),
            )));
        }
        let expected = match self
            .read_view(view, |state| state.tracked.then(|| state.lease.clone()))
            .flatten()
        {
            Some(expected) => expected,
            None => {
                return done(Err(main_core::shell::renderer_disposal::Failure(
                    "the recovery document is not tracked".into(),
                )));
            }
        };
        let (file_path, identity, untitled, title, save_as_default, original_path) =
            match selection {
                Selection::File(request) => {
                    let identity = (request.decision == RecoveryOpenDecision::Restore).then(|| {
                        RecoveryIdentity::File(RecoveryFileIdentity {
                            kind: self.ch.app.into(),
                            file_path: request.file_path.clone(),
                        })
                    });
                    (Some(request.file_path), identity, false, None, None, None)
                }
                Selection::Untitled(request) => {
                    let original = request
                        .save_as_default_path
                        .clone()
                        .or_else(|| match &request.copy {
                            RecoveryIdentity::File(file) => Some(file.file_path.clone()),
                            _ => None,
                        });
                    (
                        None,
                        Some(request.copy),
                        true,
                        Some(request.title),
                        request.save_as_default_path,
                        original,
                    )
                }
            };
        let grant = match (self.hooks.recovery_asset_paths)(
            &target,
            true,
            file_path.as_deref(),
            file_path.as_deref(),
        ) {
            Ok(grant) => grant,
            Err(error) => return done(Err(main_core::shell::renderer_disposal::Failure(error))),
        };
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err(main_core::shell::renderer_disposal::Failure(
                "recovery admission is unavailable".into(),
            )));
        };
        let Some(identity_to_read) = identity.clone() else {
            let spec = RecoverySeedSpec {
                target,
                identity,
                untitled,
                title,
                save_as_default,
                recovered: None,
                lease: None,
            };
            return self.commit_prepared_recovery(
                PreparedSeed {
                    spec: Some(spec),
                    view,
                    expected,
                    file_path,
                    disk: None,
                    grant,
                },
                done,
            );
        };
        let data = self
            .services
            .paths
            .path(shell_contract::runtime::PathName::UserData)
            .to_string_lossy()
            .into_owned();
        let worker_target = target.clone();
        let worker_file = file_path.clone();
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || read_recovery(worker_target, effects, data, identity_to_read, worker_file),
            move |result| {
                let (text, disk) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        return done(Err(main_core::shell::renderer_disposal::Failure(error)));
                    }
                };
                let recovered = crate::text_recovery::RecoveredText {
                    text,
                    original_path,
                };
                let spec = RecoverySeedSpec {
                    target,
                    identity,
                    untitled,
                    title,
                    save_as_default,
                    recovered: Some(recovered),
                    lease: None,
                };
                docs.commit_prepared_recovery(
                    PreparedSeed {
                        spec: Some(spec),
                        view,
                        expected,
                        file_path,
                        disk,
                        grant,
                    },
                    done,
                );
            },
        );
    }
}

impl TextDocs {
    pub(super) fn commit_prepared_recovery(
        self: &Rc<Self>,
        mut prepared: PreparedSeed,
        done: RecoveryOpenDone,
    ) {
        let view = prepared.view;
        let new_lease = RendererGeneration::new();
        if let Some(spec) = prepared.spec.as_mut() {
            spec.lease = Some(new_lease.clone());
        }
        let mut revoked: Option<CancelToken> = None;
        let mut validate = || {
            if self.owned_saves.borrow().contains_key(&view) {
                return Err("a document save is already pending".into());
            }
            let views = self.views.borrow();
            let state = views
                .get(&view)
                .ok_or("the recovery document is not tracked")?;
            if !state.tracked
                || !state.lease.same_generation(&prepared.expected)
                || !state.lease.is_active()
            {
                return Err("the recovery document changed before its seed was committed".into());
            }
            Ok(())
        };
        let mut commit = || {
            let mut views = self.views.borrow_mut();
            let state = views
                .get_mut(&view)
                .expect("the recovery document was validated in the same core admission");
            if let Some(path) = &prepared.file_path {
                state.open = Some(path.clone());
                state.save = Some(path.clone());
                state.allowed.insert(path.clone());
                if let Some(disk) = prepared.disk.take() {
                    state.disk.insert(path.clone(), disk);
                }
            } else {
                state.open = None;
                state.save = None;
                state.allowed.clear();
            }
            let previous = std::mem::replace(&mut state.lease, new_lease.clone());
            revoked = Some(previous.revoke_deferred());
        };
        let result = self.recovery.commit_recovery_seed(
            &mut prepared.spec,
            &mut *prepared.grant,
            &mut validate,
            &mut commit,
        );
        drop(commit);
        drop(validate);
        if let Some(revoked) = revoked {
            revoked.cancel();
        }
        match result {
            Ok(true) => done(Ok(())),
            Err(error) => done(Err(main_core::shell::renderer_disposal::Failure(error))),
            Ok(false) => {
                let docs = Rc::clone(self);
                self.recovery.after_recovery_store_available(move || {
                    docs.commit_prepared_recovery(prepared, done)
                });
            }
        }
    }
}
