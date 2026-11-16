use std::rc::Rc;
use std::sync::Arc;

use main_core::shell::tab_manager::{TabError, TabResult, ViewSpec};
use shell_contract::ipc::{
    NativeRequestOwner, NativeSavedInput, NativeSavedPath, NativeSavedTarget,
};
use shell_contract::runtime::{CancelToken, Views};
use shell_ipc_w::tabs::ShellWindow;
use shell_ipc_w::tabs::host::{
    CapturedSavedShell, EditorSeedRollbackStore, NativeBrowserBinding, NativeBrowserRegistration,
    PreparedSavedView, SavedViewLoaded,
};

use super::Hooks;

pub(super) struct SavedActorReceipt(
    pub(super) Rc<dyn shell_contract::editors::SavedActorRegistration>,
);
impl shell_ipc_w::tabs::host::EditorSeedRollback for SavedActorReceipt {
    fn retire_closed(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.0.retire_closed(
            proof,
            Box::new(move |result| done(result.map_err(|error| error.message))),
        );
    }
    fn retire_adopted(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.0.retire_closed(
            proof,
            Box::new(move |result| done(result.map_err(|error| error.message))),
        );
    }
    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.0.retire_superseded(
            proof,
            Box::new(move |result| done(result.map_err(|error| error.message))),
        );
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        self.0.rollback(Box::new(move |result| {
            done(result.map_err(|error| error.message))
        }));
    }
}

struct Captured {
    hooks: Hooks,
    shell: ShellWindow,
    owner: NativeRequestOwner,
    window: Arc<crate::views::NativeWindowLifecycle>,
}

pub(super) fn capture(
    hooks: &Hooks,
    shell: ShellWindow,
    owner: NativeRequestOwner,
) -> Result<Rc<dyn CapturedSavedShell>, String> {
    if owner.view != shell.view {
        return Err("The saved shell owner does not match its window".into());
    }
    let generation = owner
        .context
        .renderer()
        .ok_or("The saved shell renderer is unavailable")?;
    let loss = owner
        .loss_token()
        .ok_or("The saved shell lifetime is unavailable")?;
    let captured = crate::ipc::native_owner::with_owner_effect(&owner, &loss, &mut || {
        hooks
            .cx
            .views
            .view_window_captured(shell.view, generation)
            .map_err(|error| shell_contract::IpcError {
                name: Some("Error".into()),
                message: error,
            })
    })
    .ok_or("The saved shell owner was released")?
    .map_err(|error| error.message)?;
    if captured.0 != shell.window {
        return Err("The saved shell window changed".into());
    }
    Ok(Rc::new(Captured {
        hooks: Hooks {
            services: hooks.services.clone(),
            provided: hooks.provided.clone(),
            cx: hooks.cx.clone(),
        },
        shell,
        owner,
        window: captured.1,
    }))
}

impl CapturedSavedShell for Captured {
    fn prepare_fresh(
        &self,
        spec: &ViewSpec<'_>,
        input: NativeSavedInput,
        path: NativeSavedPath,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        rollback: EditorSeedRollbackStore,
    ) -> TabResult<PreparedSavedView> {
        let shell_generation = self
            .owner
            .context
            .renderer()
            .ok_or_else(|| TabError("The saved shell renderer is unavailable".into()))?;
        if !shell_generation.is_active() || self.shell.view != self.owner.view {
            return Err(TabError("The saved shell was replaced".into()));
        }
        let mut ordinary = super::OrdinaryActorStages::default();
        let mut prepared = self.hooks.prepare_editor_view_source(
            spec,
            true,
            |_| Ok(()),
            None,
            None,
            false,
            Some((self.shell.window, self.window.clone())),
            Some(&input),
            &mut ordinary,
        )?;
        let view = prepared.view;
        let generation = prepared.generation.clone();
        let mut unstarted =
            super::native_command::Unstarted(Some((prepared.cx.clone(), view, generation.clone())));
        if !ordinary.is_empty() {
            return Err(TabError(
                "The saved view unexpectedly staged an ordinary actor".into(),
            ));
        }
        let target = NativeSavedTarget::prepare(
            input,
            path,
            prepared.surface,
            view,
            generation.clone(),
            prepared.proof.reservation().clone(),
            prepared.proof.owner().clone(),
            participants.clone(),
            cancel.clone(),
            pending.clone(),
            additional.clone(),
        )
        .map_err(TabError)?;
        let actor = self
            .hooks
            .provided
            .all::<dyn shell_contract::editors::DocumentEditors>()
            .into_iter()
            .find(|editor| editor.saved_surface() == prepared.surface)
            .ok_or_else(|| TabError("The saved editor actor is unavailable".into()))?;
        let (mut barrier, docs_lease) = if prepared.surface == shell_contract::Surface::Docs {
            let docs = self
                .hooks
                .editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs")?;
            let (token, lease) = docs
                .prepare_saved_actor(target.clone())
                .map_err(|error| TabError(error.message))?;
            (token, Some(lease))
        } else {
            (
                actor
                    .prepare_saved_document(target.clone())
                    .map_err(|error| TabError(error.message))?,
                None,
            )
        };
        rollback
            .borrow_mut()
            .try_reserve(2)
            .map_err(|error| TabError(error.to_string()))?;
        let browser_owner = prepared
            .cx
            .views
            .prepare_browser_owner(view, &generation, target.browser())
            .map_err(TabError)?;
        let proof = prepared.proof.clone();
        let watched = proof.clone();
        let closure: main_core::shell::tab_manager::ViewClosureWatch<
            shell_contract::ipc::NativeBrowserRetirement,
        > = Arc::new(move |done| {
            watched.watch(Box::new(move |receipt| {
                done(shell_contract::ipc::NativeBrowserRetirement::Closed(
                    receipt,
                ))
            }))
        });
        let binding = NativeBrowserBinding {
            reservation: proof.reservation().clone(),
            owner: proof.owner().clone(),
            producer: Arc::new(proof),
            closure,
            registration: NativeBrowserRegistration::Unregistered,
        };
        let start_target = target.clone();
        let start = Box::new(
            move |adopt: shell_ipc_w::tabs::host::SavedManagerAdoption,
                  registration: shell_ipc_w::tabs::host::SavedManagerRegistration,
                  done: Box<
                dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<SavedViewLoaded>),
            >| {
                let extra: Vec<_> = additional.iter().collect();
                let mut adopt = Some(adopt);
                let admitted = crate::ipc::native_owner::with_saved_target_creation_effect(
                    &start_target,
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut |transition| {
                        let browser = browser_owner.validate(transition)?;
                        let mut register = |next: &mut dyn FnMut() -> Result<(), String>| {
                            barrier
                                .with_validated(transition, &mut || {
                                    next().map_err(|message| shell_contract::IpcError {
                                        name: Some("Error".into()),
                                        message,
                                    })
                                })
                                .map_err(|error| error.message)
                        };
                        adopt
                            .take()
                            .ok_or("The saved manager adoption was already consumed")?(
                            &start_target,
                            transition,
                            &mut register,
                        )?;
                        browser.commit();
                        Ok(())
                    },
                );
                if let Err(error) = admitted {
                    unstarted.0.take();
                    return super::hooks::fail_prepared_view(
                        prepared,
                        TabError(error),
                        Box::new(move |failure| done(Err(failure))),
                    );
                }
                let installed = (|| -> Result<(), String> {
                    let receipt = barrier.finish();
                    rollback
                        .borrow_mut()
                        .push(Rc::new(SavedActorReceipt(receipt)));
                    if let Some(lease) = docs_lease {
                        prepared
                            .cx
                            .views
                            .set_docs_media_lease(view, lease.clone())?;
                        let session = crate::host::document_assets::register_session(
                            view,
                            crate::host::document_assets::AssetKind::Docs,
                        )?;
                        prepared.cx.views.set_docs_asset_session(view, session)?;
                        prepared.docs_lease = Some(lease);
                    }
                    Ok(())
                })();
                if let Err(error) = installed {
                    unstarted.0.take();
                    return super::hooks::fail_prepared_view(
                        prepared,
                        TabError(error),
                        Box::new(move |failure| done(Err(failure))),
                    );
                }
                unstarted.0.take();
                let capture_actor = actor.clone();
                super::saved_creation::start(
                    prepared,
                    start_target,
                    participants,
                    cancel,
                    pending,
                    additional,
                    Box::new(move |view, owner| capture_actor.capture_document_owner(view, owner)),
                    registration,
                    Box::new(move |document, reply| actor.load_saved_document(document, reply)),
                    Box::new(move |result| {
                        done(
                            result.map(|ready| shell_ipc_w::tabs::host::ViewCreationReady {
                                view: SavedViewLoaded {
                                    document: ready.view.document,
                                    outcome: ready.view.outcome,
                                },
                                cleanup: ready.cleanup,
                                closure: ready.closure,
                                reservation: ready.reservation,
                            }),
                        )
                    }),
                );
            },
        );
        Ok(PreparedSavedView {
            target,
            binding,
            start,
        })
    }
}
