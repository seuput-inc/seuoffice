//! Captured-authority startup effects; completions are actual operation outcomes.
use super::{LaunchActions, Manager};
use main_core::shell::recovery_shell::{LaunchNeed, LaunchReply, UnloadedTabSpec};
use main_core::shell::renderer_disposal::Failure;
use shell_contract::{
    Provided,
    hosts::OwnedEffectsHost,
    ipc::{NativeInvocationOperation, NativeInvocationSelection, NativeRequestOwner},
    runtime::{CancelToken, Services},
};
use shell_ipc_w::tabs::TabActivationAuthority;
use std::rc::Rc;

/// Identity prefilter only; the effect still requires native operation admission.
fn same_startup_owner(actual: &NativeRequestOwner, original: &NativeRequestOwner) -> bool {
    actual.view == original.view
        && std::sync::Arc::ptr_eq(&actual.context.as_arc(), &original.context.as_arc())
        && actual
            .loss_token()
            .zip(original.loss_token())
            .is_some_and(|(actual, original)| actual.same_token(&original))
        && actual
            .context
            .renderer()
            .zip(original.context.renderer())
            .is_some_and(|(actual, original)| actual.same_generation(original))
}

pub(crate) struct StartupActions {
    services: Services,
    manager: Rc<Manager>,
    startup: NativeInvocationOperation,
    owner: NativeRequestOwner,
    shutdown: CancelToken,
    reveal: super::reveal::CapturedReveal,
}

impl StartupActions {
    pub(crate) fn new(
        services: Services,
        _provided: Rc<Provided>,
        cx: crate::host::HostContext,
        manager: Rc<Manager>,
        startup: NativeInvocationOperation,
        owner: NativeRequestOwner,
        shutdown: CancelToken,
    ) -> Result<Rc<Self>, String> {
        if !startup.shutdown().same_token(&shutdown) {
            return Err("Startup shutdown scope does not match".into());
        }
        let effects = services
            .hosts
            .get::<OwnedEffectsHost>()
            .ok_or("Startup ownership service is unavailable")?;
        effects.with_native_operation_effect(
            &startup,
            &owner,
            &[],
            startup.cancel(),
            &shutdown,
            &mut || Ok(()),
        )?;
        let sender = services
            .views
            .sender_of(owner.view)
            .ok_or("The startup shell view is unavailable")?;
        if sender.window != manager.owner {
            return Err("The startup shell window changed".into());
        }
        let reveal = super::reveal::capture(&cx, manager.owner, &owner)?;
        Ok(Rc::new(Self {
            services,
            manager,
            startup,
            owner,
            shutdown,
            reveal,
        }))
    }

    pub(crate) fn launch_actions(self: &Rc<Self>) -> LaunchActions {
        let actions = self.clone();
        LaunchActions {
            run: Rc::new(move |need, done| actions.clone().run(need, done)),
        }
    }

    pub(super) fn startup_write_admission(
        &self,
    ) -> Result<main_core::packages::electron_utils::atomic_write::owned::Admission, String> {
        let effects = self
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or("Startup ownership service is unavailable")?;
        let startup = self.startup.clone();
        let owner = self.owner.clone();
        let pending = self.shutdown.clone();
        Ok(std::sync::Arc::new(move |effect| {
            let mut effect = Some(effect);
            let mut result = None;
            effects
                .with_native_operation_effect(
                    &startup,
                    &owner,
                    &[],
                    startup.cancel(),
                    &pending,
                    &mut || {
                        let effect = effect
                            .take()
                            .ok_or("Startup write admission was repeated")?;
                        result = Some(effect());
                        Ok(())
                    },
                )
                .map_err(|message| main_core::node_fs::NodeError {
                    code: None,
                    message,
                })?;
            result.ok_or_else(|| main_core::node_fs::NodeError {
                code: None,
                message: "Startup write was not admitted".into(),
            })
        }))
    }

    fn pending(&self) -> CancelToken {
        CancelToken::default()
    }

    fn add(self: &Rc<Self>, spec: UnloadedTabSpec, done: Box<dyn FnOnce(Result<String, Failure>)>) {
        let spec = match self.manager.spec(spec) {
            Ok(spec) => spec,
            Err(error) => return done(Err(error)),
        };
        let opener = spec.opener;
        self.manager.tabs.startup_add_unloaded(
            spec,
            self.startup.clone(),
            self.owner.clone(),
            self.startup.cancel().clone(),
            self.pending(),
            Box::new(super::super::recovery_openers::bind),
            Box::new(move || super::super::recovery_openers::unregister(opener)),
            Box::new(move |result| {
                done(result.map_err(Failure).and_then(|result| {
                    result
                        .id
                        .ok_or_else(|| Failure("The startup tab was not inserted".into()))
                }))
            }),
        );
    }

    fn hydrate(
        self: &Rc<Self>,
        specs: Vec<UnloadedTabSpec>,
        done: Box<dyn FnOnce(Result<Vec<String>, Failure>)>,
    ) {
        let mut selected = Vec::with_capacity(specs.len());
        for spec in specs {
            match self.manager.spec(spec) {
                Ok(spec) => selected.push(spec),
                Err(error) => {
                    for spec in selected {
                        super::super::recovery_openers::unregister(spec.opener);
                    }
                    return done(Err(error));
                }
            }
        }
        let openers: Vec<_> = selected.iter().map(|spec| spec.opener).collect();
        let rejected = openers.clone();
        self.manager.tabs.startup_hydrate(
            selected,
            self.startup.clone(),
            self.owner.clone(),
            self.startup.cancel().clone(),
            self.pending(),
            Box::new(move |bindings| {
                for (opener, id) in bindings {
                    super::super::recovery_openers::bind(*opener, id)?;
                }
                Ok(())
            }),
            Box::new(move || {
                for opener in rejected {
                    super::super::recovery_openers::unregister(opener);
                }
            }),
            Box::new(move |result| done(result.map_err(Failure))),
        );
    }

    fn session_changed(self: &Rc<Self>, done: Box<dyn FnOnce(Result<(), Failure>)>) {
        let result = (|| {
            let effects = self
                .services
                .hosts
                .get::<OwnedEffectsHost>()
                .cloned()
                .ok_or("Startup ownership service is unavailable")?;
            let (strip, detached) = self.manager.tabs.session_snapshot()?;
            let mut views = Vec::new();
            for tab in strip
                .get("tabs")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .chain(detached.iter())
            {
                if let Some(id) = tab.get("id").and_then(serde_json::Value::as_str) {
                    if let Some(view) = self.manager.tabs.tab_contents(id) {
                        views.push((id.to_owned(), view));
                    }
                }
            }
            let mut participants = Vec::new();
            for (_, view) in &views {
                if *view == self.owner.view
                    || participants
                        .iter()
                        .any(|owner: &NativeRequestOwner| owner.view == *view)
                {
                    continue;
                }
                let generation = self
                    .services
                    .views
                    .renderer_generation(*view)
                    .ok_or("A session renderer is unavailable")?;
                participants.push(effects.capture_owner(*view, &generation)?);
            }
            let snapshot = super::capture_activation_session(&strip, &detached, &views)?;
            Ok::<_, String>((effects, participants, snapshot))
        })();
        let (effects, participants, snapshot) = match result {
            Ok(value) => value,
            Err(error) => return done(Err(Failure(error))),
        };
        let publish = super::prepare_activation_session(
            snapshot,
            TabActivationAuthority::NativeOperation {
                operation: self.startup.clone(),
                owner: self.owner.clone(),
            },
            participants,
            self.startup.cancel().clone(),
            self.pending(),
            effects,
        );
        publish(Box::new(move |result| done(result.map_err(Failure))));
    }

    pub(crate) fn run(self: Rc<Self>, need: LaunchNeed, done: Box<dyn FnOnce(LaunchReply)>) {
        let cancel = self.startup.cancel().clone();
        let pending = self.pending();
        match need {
            LaunchNeed::Hydrate(specs) => self.hydrate(
                specs,
                Box::new(move |result| done(LaunchReply::Hydrated(result))),
            ),
            LaunchNeed::AddUnloaded(spec) => self.add(
                spec,
                Box::new(move |result| done(LaunchReply::Added(result))),
            ),
            LaunchNeed::SessionChanged => {
                self.session_changed(Box::new(move |result| done(LaunchReply::Completed(result))))
            }
            LaunchNeed::OpenHome => self.manager.tabs.startup_open_home(
                self.startup.clone(),
                self.owner.clone(),
                cancel,
                pending,
                Box::new(move |result| done(LaunchReply::Completed(result.map_err(Failure)))),
            ),
            LaunchNeed::Reorder { id, slot } => self.manager.tabs.startup_reorder(
                id,
                slot as f64,
                self.startup.clone(),
                self.owner.clone(),
                cancel,
                pending,
                Box::new(move |result| {
                    done(LaunchReply::Completed(result.map(|_| ()).map_err(Failure)))
                }),
            ),
            LaunchNeed::CloseTab(id) => self.manager.tabs.close_tab_startup(
                self.startup.clone(),
                self.owner.clone(),
                id,
                cancel,
                pending,
                Box::new(move |result| done(LaunchReply::Completed(result.map_err(Failure)))),
            ),
            LaunchNeed::BeginSplit {
                a,
                b,
                orientation,
                ratio,
                focus,
            } => self.manager.tabs.begin_split_startup(
                self.startup.clone(),
                self.owner.clone(),
                a,
                b,
                orientation,
                ratio,
                focus,
                cancel,
                pending,
                Box::new(move |result| done(LaunchReply::Completed(result.map_err(Failure)))),
            ),
            LaunchNeed::Activate(id) => self.manager.tabs.activate_startup_owned(
                id,
                self.startup.clone(),
                self.owner.clone(),
                cancel,
                pending,
                Box::new(move |result| done(LaunchReply::Completed(result.map_err(Failure)))),
            ),
            LaunchNeed::RestoreSplitOrHome => self.manager.tabs.restore_split_or_home_startup(
                self.startup.clone(),
                self.owner.clone(),
                cancel,
                pending,
                Box::new(move |result| done(LaunchReply::Completed(result.map_err(Failure)))),
            ),
            LaunchNeed::OpenPaths(paths) => self.clone().open_paths(
                self.startup.clone(),
                self.owner.clone(),
                paths,
                false,
                Box::new(move |result| done(LaunchReply::Completed(result))),
            ),
            LaunchNeed::Prepare { .. } | LaunchNeed::RecordDecisions(_) | LaunchNeed::Settled => {
                unreachable!("launch runtime owns worker and settlement requests")
            }
        }
    }

    pub(crate) fn open_paths(
        self: Rc<Self>,
        operation: NativeInvocationOperation,
        owner: NativeRequestOwner,
        paths: Vec<String>,
        reveal: bool,
        done: Box<dyn FnOnce(Result<(), Failure>)>,
    ) {
        let selected = match operation.selection() {
            NativeInvocationSelection::StartupLaunch { paths, .. } => paths
                .iter()
                .map(|path| path.to_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>(),
            _ => None,
        };
        if !same_startup_owner(&owner, &self.owner)
            || selected.as_ref() != Some(&paths)
            || !operation.shutdown().same_token(&self.shutdown)
        {
            return done(Err(Failure(
                "The native startup paths do not match the selected operation".into(),
            )));
        }
        if reveal {
            let current = self.clone();
            self.reveal.run(
                &self.services,
                operation.clone(),
                owner.clone(),
                Box::new(move |result| match result {
                    Ok(()) => current.open_path_at(operation, owner, paths, 0, false, done),
                    Err(error) => done(Err(Failure(error))),
                }),
            );
        } else {
            self.open_path_at(operation, owner, paths, 0, false, done);
        }
    }

    fn open_path_at(
        self: Rc<Self>,
        operation: NativeInvocationOperation,
        owner: NativeRequestOwner,
        paths: Vec<String>,
        index: usize,
        opened: bool,
        done: Box<dyn FnOnce(Result<(), Failure>)>,
    ) {
        if index == paths.len() {
            if opened {
                return done(Ok(()));
            }
            return self.manager.tabs.startup_open_home(
                operation.clone(),
                owner,
                operation.cancel().clone(),
                self.pending(),
                Box::new(move |result| done(result.map_err(Failure))),
            );
        }
        let authority = TabActivationAuthority::NativeOperation {
            operation: operation.clone(),
            owner: owner.clone(),
        };
        let path = paths[index].clone();
        let state = self.clone();
        self.manager.tabs.route_document_path_authorized(
            path,
            authority,
            operation.cancel().clone(),
            self.pending(),
            Box::new(move |result| match result {
                Ok(view) => state.open_path_at(
                    operation,
                    owner,
                    paths,
                    index + 1,
                    opened || view.is_some(),
                    done,
                ),
                Err(error) => done(Err(Failure(error))),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::{ViewId, ipc::NativeContext, runtime::RendererGeneration};

    #[test]
    fn startup_paths_keep_the_original_captured_owner() {
        let generation = RendererGeneration::new();
        let loss = CancelToken::default();
        let original = NativeRequestOwner {
            view: ViewId(3),
            context: NativeContext::with_owner_loss((), generation.clone(), loss.clone()),
        };
        assert!(same_startup_owner(&original.clone(), &original));
        let different_context = NativeRequestOwner {
            view: ViewId(3),
            context: NativeContext::with_owner_loss((), generation.clone(), loss),
        };
        assert!(!same_startup_owner(&different_context, &original));
        let mut different_view = original.clone();
        different_view.view = ViewId(4);
        assert!(!same_startup_owner(&different_view, &original));
    }
}
