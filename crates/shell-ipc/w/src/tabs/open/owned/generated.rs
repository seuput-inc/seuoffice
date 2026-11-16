use super::*;
use main_core::packages::electron_utils::generate_handoff::{
    GenerateConvertTarget, GenerateHandoffTarget,
};
use main_core::shell::projects::new_documents::{NewDocumentOpener, NewDocumentPlacement};
use main_core::shell::tab_manager::TabError;
use shell_contract::hosts::OwnedEffectsHost;
use std::sync::atomic::{AtomicU64, Ordering};

pub enum GeneratedEditorOpen {
    Generate {
        target: GenerateHandoffTarget,
        handoff: serde_json::Value,
        opener: Option<NewDocumentOpener>,
    },
    Conversion {
        target: GenerateConvertTarget,
        document: serde_json::Value,
        opener: Option<NewDocumentOpener>,
    },
}

pub struct CommittedGeneratedTab {
    tab_id: String,
    target: NativeRequestOwner,
    created: RendererGeneration,
    effects: OwnedEffectsHost,
}

impl CommittedGeneratedTab {
    pub fn tab_id(&self) -> &str {
        &self.tab_id
    }
    pub fn view(&self) -> ViewId {
        self.target.view
    }
    pub fn created(&self) -> &RendererGeneration {
        &self.created
    }
    pub(super) fn into_target(self) -> NativeRequestOwner {
        self.target
    }
    pub fn validate_target(&self) -> Result<(), String> {
        if self.created.is_active()
            && self
                .target
                .context
                .renderer()
                .is_some_and(|renderer| renderer.same_generation(&self.created))
            && self
                .target
                .loss_token()
                .is_some_and(|loss| !loss.is_cancelled())
        {
            Ok(())
        } else {
            Err("The generated document was replaced or closed".into())
        }
    }
    pub fn with_terminal_commit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let loss = self
            .target
            .loss_token()
            .ok_or("The generated target has no native lifetime")?;
        self.effects
            .with_owned_effect(&self.target, &self.target, &loss, &loss, effect)
    }
    pub fn with_terminal_commit_owned(
        &self,
        origin: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.effects
            .with_owned_effect(origin, &self.target, cancel, pending, effect)
    }
}

pub(super) type GeneratedDone = Box<dyn FnOnce(Result<CommittedGeneratedTab, String>)>;
type PlacementDone = Box<dyn FnOnce(Result<Option<NewDocumentPlacement>, String>)>;
thread_local! { static PLACEMENTS: RefCell<HashMap<u64, PlacementDone>> = RefCell::new(HashMap::new()); }
static NEXT_PLACEMENT: AtomicU64 = AtomicU64::new(1);

impl TabsState {
    pub fn open_generated_editor_owned(
        &self,
        request: GeneratedEditorOpen,
        source: NativeRequestOwner,
        cancel: CancelToken,
        done: GeneratedDone,
    ) {
        let (request, opener) = match request {
            GeneratedEditorOpen::Generate {
                target,
                handoff,
                opener,
            } => (EditorOpen::Generate { target, handoff }, opener),
            GeneratedEditorOpen::Conversion {
                target,
                document,
                opener,
            } => (EditorOpen::Conversion { target, document }, opener),
        };
        self.begin_initialized_open(
            request,
            source,
            Vec::new(),
            cancel.clone(),
            cancel,
            opener,
            done,
        );
    }

    pub fn open_initialized_editor_owned(
        &self,
        request: EditorOpen,
        source: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        opener: Option<NewDocumentOpener>,
        done: GeneratedDone,
    ) {
        if matches!(
            request,
            EditorOpen::Generate { .. } | EditorOpen::Conversion { .. }
        ) {
            return done(Err(
                "Generate requests require the dedicated transfer API".into()
            ));
        }
        self.begin_initialized_open(request, source, participants, cancel, pending, opener, done);
    }

    fn begin_initialized_open(
        &self,
        request: EditorOpen,
        source: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        opener: Option<NewDocumentOpener>,
        done: GeneratedDone,
    ) {
        if cancel.is_cancelled() || pending.is_cancelled() {
            return done(Err(
                "The initialized tab operation is no longer active".into()
            ));
        }
        let epoch = self.manager_epoch.get();
        let has_opener = opener.is_some();
        let state = self.this.clone();
        self.generated_placement(
            opener,
            Box::new(move |placement| {
                let Some(state) = state.upgrade() else {
                    return done(Err("The tab manager is unavailable".into()));
                };
                match placement {
                    Ok(placement) => state.start_generated_open(
                        request,
                        has_opener.then_some(placement),
                        source,
                        participants,
                        cancel,
                        pending,
                        epoch,
                        done,
                    ),
                    Err(error) => done(Err(error)),
                }
            }),
        );
    }

    fn generated_placement(&self, opener: Option<NewDocumentOpener>, done: PlacementDone) {
        let view = match opener {
            None => return done(Ok(None)),
            Some(NewDocumentOpener::Home) => self.shell_window().map(|shell| shell.view),
            Some(NewDocumentOpener::Window { web_contents_id }) => {
                u32::try_from(web_contents_id).ok().map(ViewId)
            }
        };
        let Some(view) = view else {
            return done(Ok(None));
        };
        let Some(projects) = self
            .provided
            .all::<crate::projects::Projects>()
            .into_iter()
            .next()
        else {
            return done(Err("Project services are unavailable".into()));
        };
        let (seed, lookup, io) = projects.capture_new_document(view);
        let queue = main_core::shell::app_settings::queue::for_path(io.settings_path());
        let token = NEXT_PLACEMENT.fetch_add(1, Ordering::Relaxed);
        PLACEMENTS.with(|pending| {
            pending.borrow_mut().insert(token, done);
        });
        let core = self.services.core.clone();
        queue.submit(move |completion| {
            let placement = io
                .resolve(lookup)
                .map(|authority| {
                    authority.map(|authority| {
                        seed.resolve_placement(
                            authority.project_id().to_owned(),
                            authority.root().to_owned(),
                        )
                    })
                })
                .map_err(|error| error.message());
            core.post(Box::new(move || {
                let done = PLACEMENTS.with(|pending| pending.borrow_mut().remove(&token));
                drop(completion);
                if let Some(done) = done {
                    done(placement);
                }
            }));
        });
    }

    fn prepare_generated_manager_adoption(
        &self,
        operation_id: u64,
        plan: PreparedTabOpen<ViewId, super::super::super::host::RetainedSpareBinding>,
        view: ViewId,
        created: &RendererGeneration,
        retained: super::super::super::host::RetainedSpareBinding,
        epoch: u64,
    ) -> Result<
        (
            super::super::super::host::GeneratedManagerAdoption,
            super::super::super::host::GeneratedManagerRegistration,
            main_core::shell::tab_manager::GeneratedRemovalPreview<
                ViewId,
                super::super::super::host::RetainedSpareBinding,
            >,
            std::rc::Rc<RefCell<Option<String>>>,
            Box<
                dyn FnOnce() -> Option<
                    PreparedTabOpen<ViewId, super::super::super::host::RetainedSpareBinding>,
                >,
            >,
        ),
        (
            PreparedTabOpen<ViewId, super::super::super::host::RetainedSpareBinding>,
            String,
        ),
    > {
        if !retained.browser.reservation.matches(view, created)
            || !retained
                .browser
                .owner
                .reservation()
                .same_reservation(&retained.browser.reservation)
            || plan.reserved_binding().is_some_and(|original| {
                !original
                    .browser
                    .reservation
                    .same_reservation(&retained.browser.reservation)
                    || original.browser.owner.same_owner(&retained.browser.owner)
            })
        {
            return Err((
                plan,
                "The generated browser binding does not match its reservation".into(),
            ));
        }
        let original = plan
            .reserved_binding()
            .map(|binding| binding.browser.owner.clone());
        let prepared = {
            let mut inner = self.inner.borrow_mut();
            let super::super::super::state::Inner {
                manager, bridge, ..
            } = &mut *inner;
            match manager.as_ref() {
                Some(manager) => {
                    manager
                        .prepare_editor_adoption(bridge, plan, view)
                        .map_err(|failure| {
                            let (plan, error) = *failure;
                            (plan, error.0)
                        })
                }
                None => Err((plan, "The shell window is unavailable".into())),
            }
        }?;
        let commit = {
            let mut inner = self.inner.borrow_mut();
            match inner.manager.as_mut() {
                Some(manager) => manager.prepare_editor_adoption_commit(&prepared),
                None => Err("The shell window is unavailable".into()),
            }
        };
        let commit = match commit {
            Ok(commit) => commit,
            Err(error) => return Err((prepared.into_plan(), error)),
        };
        let preview = {
            let inner = self.inner.borrow();
            match inner.manager.as_ref() {
                Some(manager) => {
                    manager.preview_prepared_generated_adoption(&prepared, &commit, &retained)
                }
                None => Err("The shell window is unavailable".into()),
            }
        };
        let preview = match preview {
            Ok(preview) => preview,
            Err(error) => return Err((prepared.into_plan(), error)),
        };
        let mut loaded_id = Some(commit.tab_id().to_owned());
        let prepared = std::rc::Rc::new(RefCell::new(Some(prepared)));
        let adoption_prepared = prepared.clone();
        let recover = Box::new(move || {
            prepared
                .borrow_mut()
                .take()
                .map(|prepared| prepared.into_plan())
        });
        let binding = retained.browser.clone();
        let old_binding = retained.clone();
        let mut registered_binding = retained.clone();
        let registration_binding = binding.clone();
        let tab_id = std::rc::Rc::new(RefCell::new(None));
        let admitted_id = tab_id.clone();
        let registered_id = tab_id.clone();
        let registration_state = self.rc();
        let state = self.rc();
        let adopt = Box::new(
            move |transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
                  owner: &shell_contract::ipc::NativeBrowserOwner,
                  actor: &mut dyn FnMut(
                &mut dyn FnMut() -> Result<(), String>,
            ) -> Result<(), String>| {
                let matching_owner = match &original {
                    Some(original) => {
                        transition.matches_owner(original) && transition.proposes_owner(owner)
                    }
                    None => transition.matches_owner(owner),
                };
                if state.manager_epoch.get() != epoch
                    || state.creation_shutdown.get()
                    || !transition.matches(&binding.reservation)
                    || !owner.same_owner(&binding.owner)
                    || !matching_owner
                {
                    return Err(
                        "The generated editor adoption no longer matches its reservation".into(),
                    );
                }
                let mut pending = state.pending_owned.borrow_mut();
                let entry = pending
                    .get_mut(&operation_id)
                    .ok_or("The generated tab operation was cancelled")?;
                if entry.epoch != epoch
                    || !matches!(&entry.step, Step::GeneratedFailed { plan: None, view: pending_view } if *pending_view == view)
                {
                    return Err("The generated tab operation changed before adoption".into());
                }
                let mut inner = state.inner.borrow_mut();
                let manager = inner
                    .manager
                    .as_mut()
                    .ok_or("The shell window is unavailable")?;
                {
                    let prepared = adoption_prepared.borrow();
                    manager.validate_editor_adoption_commit(
                        prepared
                            .as_ref()
                            .ok_or("The generated manager adoption was already consumed")?,
                        &commit,
                    )?;
                }
                let mut commit = Some(commit);
                let mut retained = Some(retained);
                actor(&mut || {
                    let prepared = adoption_prepared
                        .borrow_mut()
                        .take()
                        .ok_or("The generated manager adoption was already consumed")?;
                    let commit = commit
                        .take()
                        .ok_or("The generated manager commit was already consumed")?;
                    let retained = retained
                        .take()
                        .ok_or("The generated browser binding was already consumed")?;
                    let loaded_id = loaded_id
                        .take()
                        .ok_or("The generated tab identity was already consumed")?;
                    *admitted_id.borrow_mut() =
                        Some(manager.commit_validated_editor_adoption(prepared, retained, commit));
                    entry.step = Step::GeneratedLoaded {
                        id: loaded_id,
                        view,
                    };
                    Ok(())
                })
            },
        );
        let register = Box::new(
            move |owner: &NativeRequestOwner,
                  transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
                  register: &mut dyn FnMut() -> Result<(), String>| {
                if registration_state.manager_epoch.get() != epoch
                    || registration_state.creation_shutdown.get()
                    || owner.view != view
                    || !transition.matches(&registration_binding.reservation)
                    || !transition.matches_owner(&registration_binding.owner)
                    || !owner.context.renderer().is_some_and(|created| {
                        registration_binding.reservation.matches(view, created)
                    })
                {
                    return Err(
                        "The generated editor registration no longer matches its reservation"
                            .into(),
                    );
                }
                let id = registered_id.borrow();
                let id = id
                    .as_deref()
                    .ok_or("The generated editor was not adopted")?;
                registered_binding.browser.registration =
                    super::super::super::host::NativeBrowserRegistration::Registered(owner.clone());
                let mut inner = registration_state.inner.borrow_mut();
                let manager = inner
                    .manager
                    .as_mut()
                    .ok_or("The shell window is unavailable")?;
                manager.replace_tab_binding_admitted(
                    id,
                    &view,
                    &old_binding,
                    registered_binding,
                    register,
                )
            },
        );
        Ok((adopt, register, preview, tab_id, recover))
    }

    /// The shell, survivors and participants a generated-style cleanup must
    /// capture before the new editor exists.
    pub(super) fn capture_generated_cleanup_context(
        &self,
        source: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
    ) -> Result<
        (
            (super::super::super::host::ShellWindow, NativeRequestOwner),
            Vec<GeneratedCapturedTab>,
            Vec<NativeRequestOwner>,
        ),
        String,
    > {
        let cleanup_shell = (|| -> Result<_, String> {
            let shell = self
                .shell_window()
                .ok_or("The shell window is unavailable")?;
            let generation = self
                .services
                .views
                .renderer_generation(shell.view)
                .ok_or("The shell renderer is unavailable")?;
            let effects = self
                .services
                .hosts
                .get::<OwnedEffectsHost>()
                .ok_or("Owned cleanup is unavailable")?;
            effects
                .capture_owner(shell.view, &generation)
                .map(|owner| (shell, owner))
        })();
        let cleanup_shell = cleanup_shell?;
        let captured_tabs = (|| -> Result<Vec<GeneratedCapturedTab>, String> {
            let tabs = {
                let inner = self.inner.borrow();
                let manager = inner
                    .manager
                    .as_ref()
                    .ok_or("The shell window is unavailable")?;
                manager
                    .list()
                    .into_iter()
                    .map(|tab| {
                        let view = manager.web_contents_for_tab(&tab.id);
                        let unloaded = manager.is_tab_unloaded(&tab.id);
                        (tab.id, view, unloaded)
                    })
                    .collect::<Vec<_>>()
            };
            let effects = self
                .services
                .hosts
                .get::<OwnedEffectsHost>()
                .ok_or("Owned cleanup is unavailable")?;
            tabs.into_iter()
                .map(|(id, view, unloaded)| match view {
                    Some(view) => {
                        let generation = self
                            .services
                            .views
                            .renderer_generation(view)
                            .ok_or("A surviving tab renderer is unavailable")?;
                        Ok(GeneratedCapturedTab::Loaded {
                            id,
                            owner: effects.capture_owner(view, &generation)?,
                        })
                    }
                    None if unloaded => Ok(GeneratedCapturedTab::Unloaded { id }),
                    None => Ok(GeneratedCapturedTab::Viewless { id }),
                })
                .collect()
        })();
        let tabs = captured_tabs?;
        let cleanup_participants = (|| -> Result<Vec<NativeRequestOwner>, String> {
            let effects = self
                .services
                .hosts
                .get::<OwnedEffectsHost>()
                .ok_or("Owned cleanup is unavailable")?;
            let mut captured = participants.to_vec();
            for tab in &tabs {
                if let GeneratedCapturedTab::Loaded { owner, .. } = tab {
                    if owner.view != source.view
                        && !captured.iter().any(|entry| entry.view == owner.view)
                    {
                        captured.push(owner.clone());
                    }
                }
            }
            let (_, detached) = self.session_snapshot()?;
            for row in detached {
                let Some(id) = row.get("id").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(view) = self.tab_contents(id) else {
                    continue;
                };
                if view == source.view || captured.iter().any(|entry| entry.view == view) {
                    continue;
                }
                let generation = self
                    .services
                    .views
                    .renderer_generation(view)
                    .ok_or("A detached tab renderer is unavailable")?;
                captured.push(effects.capture_owner(view, &generation)?);
            }
            Ok(captured)
        })();
        let cleanup_participants = cleanup_participants?;
        Ok((cleanup_shell, tabs, cleanup_participants))
    }

    fn start_generated_open(
        &self,
        request: EditorOpen,
        placement: Option<Option<NewDocumentPlacement>>,
        source: NativeRequestOwner,
        mut participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        epoch: u64,
        done: GeneratedDone,
    ) {
        if epoch != self.manager_epoch.get() || cancel.is_cancelled() || pending.is_cancelled() {
            return done(Err(
                "The initialized tab operation is no longer active".into()
            ));
        }
        let id = match self.owned_id() {
            Ok(id) => id,
            Err(error) => return done(Err(error)),
        };
        let Some(mut plan) = self
            .inner
            .borrow_mut()
            .manager_call(|manager, host| manager.prepare_editor_open(host, request))
        else {
            return done(Err("The shell window is unavailable".into()));
        };
        let seed = plan.take_seed();
        let spare = if let Some(view) = plan.reserved_view().copied() {
            let captured = (|| {
                let binding = plan
                    .reserved_binding()
                    .cloned()
                    .ok_or("The reserved spare binding is unavailable")?;
                let owner = match &binding.browser.registration {
                    super::super::super::host::NativeBrowserRegistration::Registered(owner)
                        if owner.view == view =>
                    {
                        owner.clone()
                    }
                    _ => return Err("The reserved spare is not registered".to_owned()),
                };
                let generation = owner
                    .context
                    .renderer()
                    .cloned()
                    .ok_or("The reserved renderer is unavailable")?;
                Ok::<_, String>((view, generation, owner, binding))
            })();
            match captured {
                Ok((view, generation, owner, binding)) => {
                    participants.push(owner);
                    Some((view, generation, binding))
                }
                Err(error) => {
                    self.cancel_owned_step(Step::New(plan));
                    return done(Err(error));
                }
            }
        } else {
            None
        };
        let reserved_spare = spare.is_some();
        let (cleanup_shell, tabs, cleanup_participants) =
            match self.capture_generated_cleanup_context(&source, &participants) {
                Ok(captured) => captured,
                Err(error) => {
                    self.cancel_owned_step(Step::New(plan));
                    return done(Err(error));
                }
            };
        let rollback: super::super::super::host::EditorSeedRollbackStore = std::rc::Rc::default();
        if let Err(error) = self.begin_creation(id) {
            let state = self.rc();
            let reply = std::rc::Rc::new(RefCell::new(Some(done)));
            let failed = reply.clone();
            let message = error.clone();
            self.retain_editor_cleanup(
                None,
                None,
                rollback,
                false,
                Box::new(move |failure| {
                    if let Some(done) = failed.borrow_mut().take() {
                        done(Err(format!("{message}; cleanup failed: {failure}")));
                    }
                }),
                Box::new(move || {
                    state.cancel_owned_step(Step::New(plan));
                    if let Some(done) = reply.borrow_mut().take() {
                        done(Err(error));
                    }
                }),
            );
            return;
        }
        let prepared = match plan.creation_spec() {
            Some(spec) => self.hooks.prepare_generated_view_owned(
                &spec,
                source.clone(),
                participants.clone(),
                cancel.clone(),
                pending.clone(),
                seed,
                placement,
                rollback.clone(),
            ),
            None => match spare {
                Some((view, generation, binding)) => self.hooks.prepare_initialized_spare_owned(
                    view,
                    &generation,
                    binding,
                    source.clone(),
                    participants.clone(),
                    cancel.clone(),
                    pending.clone(),
                    seed,
                    placement,
                    rollback.clone(),
                ),
                None => Err(TabError(
                    "The reserved initialized renderer is unavailable".into(),
                )),
            },
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let state = self.rc();
                let reply = std::rc::Rc::new(RefCell::new(Some(done)));
                let failed = reply.clone();
                let message = error.0.clone();
                self.retain_editor_cleanup(
                    None,
                    None,
                    rollback,
                    false,
                    Box::new(move |failure| {
                        if let Some(done) = failed.borrow_mut().take() {
                            done(Err(format!("{message}; cleanup failed: {failure}")));
                        }
                    }),
                    Box::new(move || {
                        state.cancel_owned_step(Step::New(plan));
                        state.finish_creation(id);
                        if let Some(done) = reply.borrow_mut().take() {
                            done(Err(error.0));
                        }
                    }),
                );
                return;
            }
        };
        let super::super::super::host::PreparedGeneratedView {
            view,
            created,
            binding,
            spare_actor,
            installer,
            start,
        } = prepared;
        let retained = super::super::super::host::RetainedSpareBinding::new(
            binding.clone(),
            rollback.clone(),
            spare_actor.clone(),
        );
        let (adopt, register, preview, tab_id, recover) = match self
            .prepare_generated_manager_adoption(id, plan, view, &created, retained, epoch)
        {
            Ok(prepared) => prepared,
            Err((plan, error)) => {
                drop(start);
                drop(installer);
                let state = self.rc();
                let reply = std::rc::Rc::new(RefCell::new(Some(done)));
                let failed = reply.clone();
                let message = error.clone();
                self.retain_editor_cleanup(
                    None,
                    None,
                    rollback,
                    false,
                    Box::new(move |failure| {
                        if let Some(done) = failed.borrow_mut().take() {
                            done(Err(format!("{message}; cleanup failed: {failure}")));
                        }
                    }),
                    Box::new(move || {
                        state.cancel_owned_step(Step::New(plan));
                        state.finish_creation(id);
                        if let Some(done) = reply.borrow_mut().take() {
                            done(Err(error));
                        }
                    }),
                );
                return;
            }
        };
        let generated_cleanup = GeneratedCleanupAuthority {
            operation: id,
            source: source.clone(),
            shell: cleanup_shell.1,
            shell_window: cleanup_shell.0,
            manager_epoch: epoch,
            cleanup: self.hooks.cleanup_lifetime(),
            participants: cleanup_participants,
            survivors: tabs,
            preview,
            reservation: binding.reservation.clone(),
            browser: binding.owner.clone(),
            retirement: std::rc::Rc::new(RefCell::new(None)),
            cancel: cancel.clone(),
            pending: pending.clone(),
            rollback: rollback.clone(),
        };
        let (coordinator, cleanup) =
            match super::generated_cleanup::GeneratedCleanupCoordinator::prepare(
                self.rc(),
                generated_cleanup,
                id,
                view,
                binding,
                participants,
                spare_actor,
                tab_id,
                reserved_spare,
                installer,
            ) {
                Ok(prepared) => prepared,
                Err((_, error)) => {
                    drop(start);
                    drop(adopt);
                    drop(register);
                    let plan = recover();
                    let state = self.rc();
                    let reply = std::rc::Rc::new(RefCell::new(Some(done)));
                    let failed = reply.clone();
                    let message = error.clone();
                    self.retain_editor_cleanup(
                        None,
                        None,
                        rollback,
                        false,
                        Box::new(move |failure| {
                            if let Some(done) = failed.borrow_mut().take() {
                                done(Err(format!("{message}; cleanup failed: {failure}")));
                            }
                        }),
                        Box::new(move || {
                            if let Some(plan) = plan {
                                state.cancel_owned_step(Step::New(plan));
                            }
                            state.finish_creation(id);
                            if let Some(done) = reply.borrow_mut().take() {
                                done(Err(error));
                            }
                        }),
                    );
                    return;
                }
            };
        self.pending_owned.borrow_mut().insert(
            id,
            PendingOwnedOpen {
                epoch,
                cancel: cancel.clone(),
                generation: created,
                step: Step::GeneratedFailed { plan: None, view },
                done: Completion::Generated(done),
                generated_cleanup: Some(coordinator.clone()),
                diagram: None,
            },
        );
        let state = self.rc();
        start(
            adopt,
            register,
            cleanup,
            Box::new(move |result| {
                if let Some(plan) = recover() {
                    let plan = {
                        let mut pending = state.pending_owned.borrow_mut();
                        match pending.get_mut(&id) {
                            Some(PendingOwnedOpen {
                                step:
                                    Step::GeneratedFailed {
                                        plan: slot @ None, ..
                                    },
                                ..
                            }) => {
                                *slot = Some(plan);
                                None
                            }
                            _ => Some(plan),
                        }
                    };
                    if let Some(plan) = plan {
                        state.cancel_owned_step(Step::New(plan));
                    }
                }
                match result {
                    Ok(ready) => {
                        if !state.pending_owned.borrow().contains_key(&id) {
                            let completed = state.clone();
                            state.retain_creation_cleanup_with_rollback(
                                Some(ready.cleanup),
                                None,
                                Some(coordinator.rollback_action()),
                                Box::new(|error| {
                                    eprintln!("[shell] orphaned generated cleanup failed: {error}")
                                }),
                                Box::new(move || {
                                    eprintln!("[shell] discarded orphaned generated editor {id}");
                                    completed.finish_creation(id);
                                }),
                            );
                            return;
                        }
                        state.finish_owned_open(id, Ok(NativeOpened::Generated(ready)));
                    }
                    Err(failure) => {
                        let main_core::shell::tab_manager::ViewCreationFailure { error, cleanup } =
                            failure;
                        let (cleanup,proof) = match cleanup {
                    main_core::shell::tab_manager::ViewCreationCleanupState::Complete => (None,None),
                    main_core::shell::tab_manager::ViewCreationCleanupState::Acknowledged(proof) => (None,Some(proof)),
                    main_core::shell::tab_manager::ViewCreationCleanupState::Pending(cleanup) => (Some(cleanup),None),
                };
                        let completed = state.clone();
                        let failed = state.clone();
                        let original = error.0.clone();
                        state.retain_creation_cleanup_with_rollback(
                            cleanup,
                            proof,
                            Some(coordinator.rollback_action()),
                            Box::new(move |failure| {
                                failed.notify_pending_creation_error(
                                    id,
                                    format!("{original}; cleanup failed: {failure}"),
                                );
                            }),
                            Box::new(move || {
                                completed.finish_owned_open(id, Err(error.0));
                                completed.finish_creation(id);
                            }),
                        );
                    }
                }
            }),
        );
    }

    pub(super) fn finish_generated_open(
        &self,
        operation: u64,
        epoch: u64,
        generation: RendererGeneration,
        step: Step,
        result: Result<NativeOpened, String>,
        coordinator: Option<std::rc::Rc<super::generated_cleanup::GeneratedCleanupCoordinator>>,
        aborted: Option<String>,
        done: GeneratedDone,
    ) {
        let Some(coordinator) = coordinator else {
            self.close_owned_result(result);
            return done(Err("The generated completion has no cleanup owner".into()));
        };
        let ready = match result {
            Ok(NativeOpened::Generated(ready)) => ready,
            Err(error) => {
                self.finish_creation(operation);
                return done(Err(error));
            }
            Ok(other) => {
                self.close_owned_result(Ok(other));
                self.finish_creation(operation);
                return done(Err(
                    "The generated editor returned a mismatched completion".into()
                ));
            }
        };
        let tab_id = match step {
            Step::GeneratedLoaded { id, view }
                if view == ready.view.view()
                    && ready.view.created().same_generation(&generation)
                    && coordinator.matches_binding(&ready.reservation) =>
            {
                id
            }
            _ => {
                return self.fail_generated_completion(
                    operation,
                    ready,
                    coordinator,
                    None,
                    "The generated editor completion no longer matches its adoption".into(),
                    done,
                );
            }
        };
        if let Some(error) = aborted.or_else(|| {
            (epoch != self.manager_epoch.get())
                .then(|| "The shell window changed while opening the generated document".into())
        }) {
            return self.fail_generated_completion(
                operation,
                ready,
                coordinator,
                None,
                error,
                done,
            );
        }
        let Some(effects) = self.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return self.fail_generated_completion(
                operation,
                ready,
                coordinator,
                None,
                "Owned target completion effects are unavailable".into(),
                done,
            );
        };
        let (authority, participants, cancel, pending) = match coordinator.activation_inputs() {
            Ok(inputs) => inputs,
            Err(error) => {
                return self.fail_generated_completion(
                    operation,
                    ready,
                    coordinator,
                    None,
                    error,
                    done,
                );
            }
        };
        let target = match &ready.reservation.registration {
            super::super::super::host::NativeBrowserRegistration::Registered(target) => {
                target.clone()
            }
            super::super::super::host::NativeBrowserRegistration::Unregistered => {
                return self.fail_generated_completion(
                    operation,
                    ready,
                    coordinator,
                    None,
                    "The generated editor has no registered native owner".into(),
                    done,
                );
            }
        };
        let reply = std::rc::Rc::new(RefCell::new(Some(done)));
        let failed_reply = reply.clone();
        let on_pending = std::rc::Rc::new(move |error: &str| {
            let done = failed_reply.borrow_mut().take();
            if let Some(done) = done {
                done(Err(error.to_owned()));
            }
        });
        let state = self.rc();
        let settled_id = tab_id.clone();
        let settled = Box::new(move |result: Result<(), String>| {
            let done = reply.borrow_mut().take();
            let result = result.and_then(|()| {
                if done.is_none() {
                    return Err("The generated activation already reported a failure".into());
                }
                if epoch != state.manager_epoch.get() || !generation.is_active() {
                    return Err("The generated editor changed before activation settled".into());
                }
                let receipt = CommittedGeneratedTab {
                    tab_id: settled_id,
                    target: ready.view.into_target(),
                    created: generation.clone(),
                    effects,
                };
                receipt.validate_target()?;
                Ok(receipt)
            });
            match result {
                Err(error) => state.fail_generated_completion_parts(
                    operation,
                    ready.cleanup,
                    coordinator,
                    None,
                    error,
                    done,
                ),
                Ok(receipt) => {
                    let store = match coordinator.rollback_store() {
                        Ok(store) => store,
                        Err(error) => {
                            return state.fail_generated_completion_parts(
                                operation,
                                ready.cleanup,
                                coordinator,
                                None,
                                error,
                                done,
                            );
                        }
                    };
                    match state.retain_adopted_editor(
                        receipt.view(),
                        generation,
                        ready.reservation.reservation,
                        ready.reservation.owner,
                        ready.closure,
                        store,
                    ) {
                        Ok(handle) => {
                            if handle.is_unclosed() != Ok(true)
                                || receipt.validate_target().is_err()
                            {
                                let error = "The generated editor retired during lifetime handoff"
                                    .to_owned();
                                return state.fail_generated_completion_parts(
                                    operation,
                                    ready.cleanup,
                                    coordinator,
                                    Some(handle.into_failure(error.clone())),
                                    error,
                                    done,
                                );
                            }
                            coordinator.release();
                            state.finish_creation(operation);
                            if let Some(done) = done {
                                done(Ok(receipt));
                            }
                        }
                        Err(failure) => {
                            let error = failure.message().to_owned();
                            state.fail_generated_completion_parts(
                                operation,
                                ready.cleanup,
                                coordinator,
                                Some(failure),
                                error,
                                done,
                            );
                        }
                    }
                }
            }
        });
        self.activate_loaded_authorized(
            &tab_id,
            authority,
            target,
            participants,
            cancel,
            pending,
            Box::new(|_| {}),
            Some(on_pending),
            Some(settled),
        );
    }

    fn fail_generated_completion(
        &self,
        operation: u64,
        ready: super::super::super::host::ViewCreationReady<
            super::super::super::host::GeneratedViewCommit,
        >,
        coordinator: std::rc::Rc<super::generated_cleanup::GeneratedCleanupCoordinator>,
        lifetime: Option<AdoptedLifetimeFailure>,
        error: String,
        done: GeneratedDone,
    ) {
        self.fail_generated_completion_parts(
            operation,
            ready.cleanup,
            coordinator,
            lifetime,
            error,
            Some(done),
        );
    }

    pub(super) fn fail_generated_completion_parts<T: 'static>(
        &self,
        operation: u64,
        cleanup: super::super::super::host::ViewCreationCleanup,
        coordinator: std::rc::Rc<super::generated_cleanup::GeneratedCleanupCoordinator>,
        lifetime: Option<AdoptedLifetimeFailure>,
        error: String,
        done: Option<Box<dyn FnOnce(Result<T, String>)>>,
    ) {
        let rollback = coordinator.failed_lifetime_rollback(lifetime);
        let reply = std::rc::Rc::new(RefCell::new(done));
        let failed = reply.clone();
        let message = error.clone();
        let state = self.rc();
        self.retain_creation_cleanup_with_rollback(
            Some(cleanup),
            None,
            Some(rollback),
            Box::new(move |failure| {
                let done = failed.borrow_mut().take();
                if let Some(done) = done {
                    done(Err(format!("{message}; cleanup failed: {failure}")));
                }
            }),
            Box::new(move || {
                state.finish_creation(operation);
                let done = reply.borrow_mut().take();
                if let Some(done) = done {
                    done(Err(error));
                }
            }),
        );
    }
}
