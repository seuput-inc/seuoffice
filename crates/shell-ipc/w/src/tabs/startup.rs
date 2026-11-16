//! Startup-only prepared tab mutations, published under the captured launch operation.
use super::{
    activation::{TabActivationAuthority, driver::publish_startup_changed},
    state::TabsState,
};
use main_core::shell::tab_manager::{
    Opener, PreparedStartupMutation, StartupMutationResult, UnloadedTabSpec,
};
use shell_contract::{
    ViewId,
    hosts::OwnedEffectsHost,
    ipc::{NativeInvocationOperation, NativeRequestOwner},
    runtime::CancelToken,
};
use std::rc::Rc;

pub type StartupBind = Box<dyn FnMut(Opener, &str) -> Result<(), String>>;
pub type StartupMutationDone = Box<dyn FnOnce(Result<StartupMutationResult, String>)>;
pub type StartupHydrationBind = Box<dyn FnMut(&[(Opener, String)]) -> Result<(), String>>;

enum Mutation {
    Add(UnloadedTabSpec, StartupBind, Box<dyn FnOnce()>),
    Reorder(String, f64),
}

impl TabsState {
    pub fn startup_open_home(
        &self,
        operation: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: super::activation::TabUiDone,
    ) {
        self.activate_loaded_native_operation_many(
            "home",
            operation,
            owner.clone(),
            owner,
            Vec::new(),
            cancel,
            pending,
            done,
        );
    }
    #[allow(clippy::too_many_arguments)]
    pub fn startup_hydrate(
        &self,
        specs: Vec<UnloadedTabSpec>,
        operation: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        bind: StartupHydrationBind,
        rejected: Box<dyn FnOnce()>,
        done: Box<dyn FnOnce(Result<Vec<String>, String>)>,
    ) {
        let state = self.rc();
        let action: Box<dyn FnOnce()> = Box::new(move || {
            run_hydration(
                state, specs, operation, owner, cancel, pending, bind, rejected, done,
            )
        });
        if self.activation_running.replace(true) {
            self.activation_queue.borrow_mut().push_back(action);
        } else {
            action();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn startup_add_unloaded(
        &self,
        spec: UnloadedTabSpec,
        operation: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        bind: StartupBind,
        rejected: Box<dyn FnOnce()>,
        done: StartupMutationDone,
    ) {
        self.queue_startup_mutation(
            Mutation::Add(spec, bind, rejected),
            operation,
            owner,
            cancel,
            pending,
            done,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn startup_reorder(
        &self,
        id: String,
        index: f64,
        operation: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: StartupMutationDone,
    ) {
        self.queue_startup_mutation(
            Mutation::Reorder(id, index),
            operation,
            owner,
            cancel,
            pending,
            done,
        );
    }

    fn queue_startup_mutation(
        &self,
        mutation: Mutation,
        operation: NativeInvocationOperation,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: StartupMutationDone,
    ) {
        let state = self.rc();
        let action: Box<dyn FnOnce()> =
            Box::new(move || run(state, mutation, operation, owner, cancel, pending, done));
        if self.activation_running.replace(true) {
            self.activation_queue.borrow_mut().push_back(action);
        } else {
            action();
        }
    }
}

fn finish<T>(
    state: Rc<TabsState>,
    done: Box<dyn FnOnce(Result<T, String>)>,
    result: Result<T, String>,
) {
    let next = state.activation_queue.borrow_mut().pop_front();
    if next.is_none() {
        state.activation_running.set(false);
    }
    done(result);
    state.finish_creation_drain_if_ready();
    if let Some(next) = next {
        next();
    }
}

fn run(
    state: Rc<TabsState>,
    mut mutation: Mutation,
    operation: NativeInvocationOperation,
    owner: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    done: StartupMutationDone,
) {
    let mut rejected = None;
    let prepared = (|| -> Result<(PreparedStartupMutation<ViewId>, Vec<NativeRequestOwner>, OwnedEffectsHost), String> {
        let effects = state.services.hosts.get::<OwnedEffectsHost>().cloned().ok_or("Owned startup is unavailable")?;
        let (prepared, shell, views) = {
            let inner = state.inner.borrow();
            let manager = inner.manager.as_ref().ok_or("The shell window is unavailable")?;
            let shell = inner.bridge.shell.ok_or("The shell window is unavailable")?;
            let prepared = match &mutation {
                Mutation::Add(spec, _, _) => {
                    if spec.title.is_none() { return Err("A recovery tab must have its captured title".into()); }
                    manager.prepare_startup_add(spec.clone(), String::new())
                }
                Mutation::Reorder(id, index) => manager.prepare_startup_reorder(id, *index),
            };
            let views = manager.list().into_iter().filter_map(|tab| manager.web_contents_for_tab(&tab.id)).collect::<Vec<_>>();
            (prepared, shell, views)
        };
        let shell_owner = super::activation::driver::capture_shell_owner(&state, &effects, shell)?;
        let native = state.hooks.prepare_activation_ui(shell, &shell_owner, None, &[])?;
        let mut participants = vec![native.shell];
        let (_, detached) = state.session_snapshot()?;
        let mut views = views;
        for row in detached {
            if let Some(id) = row.get("id").and_then(serde_json::Value::as_str)
                && let Some(view) = state.tab_contents(id)
            {
                views.push(view);
            }
        }
        for view in views {
            if view == owner.view || participants.iter().any(|entry| entry.view == view) { continue; }
            let generation = state.services.views.renderer_generation(view).ok_or("A startup participant is unavailable")?;
            participants.push(effects.capture_owner(view, &generation)?);
        }
        Ok((prepared, participants, effects))
    })();
    let (prepared, participants, effects) = match prepared {
        Ok(value) => value,
        Err(error) => {
            if let Mutation::Add(_, _, on_rejected) = mutation {
                on_rejected();
            }
            return finish(state, done, Err(error));
        }
    };
    let mut prepared = Some(prepared);
    let mut outcome = None;
    let result = effects.with_native_operation_effect(
        &operation,
        &owner,
        &participants,
        &cancel,
        &pending,
        &mut || {
            let mut inner = state.inner.borrow_mut();
            let manager = inner
                .manager
                .as_mut()
                .ok_or("The shell window is unavailable")?;
            let result = match &mut mutation {
                Mutation::Add(_, bind, _) => manager.commit_startup_mutation(
                    prepared
                        .take()
                        .ok_or("Startup mutation was already committed")?,
                    bind.as_mut(),
                ),
                Mutation::Reorder(_, _) => manager.commit_startup_mutation(
                    prepared
                        .take()
                        .ok_or("Startup mutation was already committed")?,
                    &mut |_, _| Err("Unexpected startup binding".into()),
                ),
            }?;
            outcome = Some(result);
            Ok(())
        },
    );
    if let Err(error) = result {
        if let Mutation::Add(_, _, on_rejected) = mutation {
            rejected = Some(on_rejected);
        }
        if let Some(rejected) = rejected {
            rejected();
        }
        return finish(state, done, Err(error));
    }
    let Some(outcome) = outcome else {
        return finish(state, done, Err("Startup mutation was not admitted".into()));
    };
    if !outcome.changed {
        return finish(state, done, Ok(outcome));
    }
    let authority = TabActivationAuthority::NativeOperation { operation, owner };
    let completion_state = state.clone();
    publish_startup_changed(
        state,
        authority,
        participants,
        cancel,
        pending,
        Box::new(move |result| {
            finish(completion_state, done, result.map(|()| outcome));
        }),
    );
}

#[allow(clippy::too_many_arguments)]
fn run_hydration(
    state: Rc<TabsState>,
    specs: Vec<UnloadedTabSpec>,
    operation: NativeInvocationOperation,
    owner: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    mut bind: StartupHydrationBind,
    rejected: Box<dyn FnOnce()>,
    done: Box<dyn FnOnce(Result<Vec<String>, String>)>,
) {
    let prepared = (|| {
        let effects = state
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or("Owned startup is unavailable")?;
        let (prepared, shell, mut views) = {
            let inner = state.inner.borrow();
            let manager = inner
                .manager
                .as_ref()
                .ok_or("The shell window is unavailable")?;
            let shell = inner
                .bridge
                .shell
                .ok_or("The shell window is unavailable")?;
            let views = manager
                .list()
                .into_iter()
                .filter_map(|tab| manager.web_contents_for_tab(&tab.id))
                .collect::<Vec<_>>();
            (manager.prepare_startup_hydrate(specs), shell, views)
        };
        let shell_owner = super::activation::driver::capture_shell_owner(&state, &effects, shell)?;
        let native = state
            .hooks
            .prepare_activation_ui(shell, &shell_owner, None, &[])?;
        let mut participants = vec![native.shell];
        let (_, detached) = state.session_snapshot()?;
        for row in detached {
            if let Some(id) = row.get("id").and_then(serde_json::Value::as_str)
                && let Some(view) = state.tab_contents(id)
            {
                views.push(view);
            }
        }
        for view in views {
            if view == owner.view || participants.iter().any(|entry| entry.view == view) {
                continue;
            }
            let generation = state
                .services
                .views
                .renderer_generation(view)
                .ok_or("A startup participant is unavailable")?;
            participants.push(effects.capture_owner(view, &generation)?);
        }
        Ok::<_, String>((prepared, participants, effects))
    })();
    let (prepared, participants, effects) = match prepared {
        Ok(value) => value,
        Err(error) => {
            rejected();
            return finish(state, done, Err(error));
        }
    };
    let mut prepared = Some(prepared);
    let mut ids = None;
    let result = effects.with_native_operation_effect(
        &operation,
        &owner,
        &participants,
        &cancel,
        &pending,
        &mut || {
            let mut inner = state.inner.borrow_mut();
            let manager = inner
                .manager
                .as_mut()
                .ok_or("The shell window is unavailable")?;
            ids = Some(manager.commit_startup_hydrate(
                prepared.take().ok_or("Hydration was already committed")?,
                bind.as_mut(),
            )?);
            Ok(())
        },
    );
    if let Err(error) = result {
        rejected();
        return finish(state, done, Err(error));
    }
    let Some(ids) = ids else {
        rejected();
        return finish(state, done, Err("Hydration was not admitted".into()));
    };
    let completion_state = state.clone();
    publish_startup_changed(
        state,
        TabActivationAuthority::NativeOperation { operation, owner },
        participants,
        cancel,
        pending,
        Box::new(move |result| finish(completion_state, done, result.map(|()| ids))),
    );
}
