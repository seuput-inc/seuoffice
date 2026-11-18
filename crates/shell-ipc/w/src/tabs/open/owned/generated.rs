use super::*;
use main_core::packages::electron_utils::generate_handoff::{GenerateConvertTarget, GenerateHandoffTarget};
use main_core::shell::projects::new_documents::{NewDocumentOpener, NewDocumentPlacement};
use main_core::shell::tab_manager::{EditorSeed, TabError};
use shell_contract::hosts::OwnedEffectsHost;
use std::sync::atomic::{AtomicU64, Ordering};

pub enum GeneratedEditorOpen {
    Generate { target: GenerateHandoffTarget, handoff: serde_json::Value, opener: Option<NewDocumentOpener> },
    Conversion { target: GenerateConvertTarget, document: serde_json::Value, opener: Option<NewDocumentOpener> },
}

pub struct CommittedGeneratedTab {
    tab_id: String,
    target: NativeRequestOwner,
    created: RendererGeneration,
    effects: OwnedEffectsHost,
}

impl CommittedGeneratedTab {
    pub fn tab_id(&self) -> &str { &self.tab_id }
    pub fn view(&self) -> ViewId { self.target.view }
    pub fn created(&self) -> &RendererGeneration { &self.created }
    pub(super) fn into_target(self) -> NativeRequestOwner { self.target }
    pub fn validate_target(&self) -> Result<(), String> {
        if self.created.is_active()
            && self.target.context.renderer().is_some_and(|renderer| renderer.same_generation(&self.created))
            && self.target.loss_token().is_some_and(|loss| !loss.is_cancelled()) {
            Ok(())
        } else { Err("The generated document was replaced or closed".into()) }
    }
    pub fn with_terminal_commit(&self, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        let loss = self.target.loss_token().ok_or("The generated target has no native lifetime")?;
        self.effects.with_owned_effect(&self.target, &self.target, &loss, &loss, effect)
    }
    pub fn with_terminal_commit_owned(
        &self, origin:&NativeRequestOwner, cancel:&CancelToken, pending:&CancelToken,
        effect:&mut dyn FnMut()->Result<(),String>,
    )->Result<(),String> {
        self.effects.with_owned_effect(origin,&self.target,cancel,pending,effect)
    }
}

pub(super) type GeneratedDone = Box<dyn FnOnce(Result<CommittedGeneratedTab, String>)>;
type PlacementDone = Box<dyn FnOnce(Result<Option<NewDocumentPlacement>, String>)>;
thread_local! { static PLACEMENTS: RefCell<HashMap<u64, PlacementDone>> = RefCell::new(HashMap::new()); }
static NEXT_PLACEMENT: AtomicU64 = AtomicU64::new(1);

impl TabsState {
    pub fn open_generated_editor_owned(&self, request: GeneratedEditorOpen, source: NativeRequestOwner, cancel: CancelToken, done: GeneratedDone) {
        let (request, opener) = match request {
            GeneratedEditorOpen::Generate { target, handoff, opener } => (EditorOpen::Generate { target, handoff }, opener),
            GeneratedEditorOpen::Conversion { target, document, opener } => (EditorOpen::Conversion { target, document }, opener),
        };
        self.begin_initialized_open(request, source, Vec::new(), cancel.clone(), cancel, opener, done);
    }

    pub fn open_initialized_editor_owned(
        &self, request: EditorOpen, source: NativeRequestOwner, participants: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, opener: Option<NewDocumentOpener>, done: GeneratedDone,
    ) {
        if matches!(request, EditorOpen::Generate { .. } | EditorOpen::Conversion { .. }) {
            return done(Err("Generate requests require the dedicated transfer API".into()));
        }
        self.begin_initialized_open(request, source, participants, cancel, pending, opener, done);
    }

    fn begin_initialized_open(
        &self, request: EditorOpen, source: NativeRequestOwner, participants: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, opener: Option<NewDocumentOpener>, done: GeneratedDone,
    ) {
        let epoch = self.manager_epoch.get();
        let has_opener = opener.is_some();
        let state = self.this.clone();
        self.generated_placement(opener, Box::new(move |placement| {
            let Some(state) = state.upgrade() else { return done(Err("The tab manager is unavailable".into())); };
            match placement {
                Ok(placement) => state.start_generated_open(request, has_opener.then_some(placement), source, participants, cancel, pending, epoch, done),
                Err(error) => done(Err(error)),
            }
        }));
    }

    fn generated_placement(&self, opener: Option<NewDocumentOpener>, done: PlacementDone) {
        let view = match opener {
            None => return done(Ok(None)),
            Some(NewDocumentOpener::Home) => self.shell_window().map(|shell| shell.view),
            Some(NewDocumentOpener::Window { web_contents_id }) => u32::try_from(web_contents_id).ok().map(ViewId),
        };
        let Some(view) = view else { return done(Ok(None)); };
        let Some(projects) = self.provided.all::<crate::projects::Projects>().into_iter().next() else {
            return done(Err("Project services are unavailable".into()));
        };
        let (seed, lookup, io) = projects.capture_new_document(view);
        let queue = main_core::shell::app_settings::queue::for_path(io.settings_path());
        let token = NEXT_PLACEMENT.fetch_add(1, Ordering::Relaxed);
        PLACEMENTS.with(|pending| { pending.borrow_mut().insert(token, done); });
        let core = self.services.core.clone();
        queue.submit(move |completion| {
            let placement = io.resolve(lookup).map(|authority| authority.map(|authority| {
                seed.resolve_placement(authority.project_id().to_owned(), authority.root().to_owned())
            })).map_err(|error| error.message());
            core.post(Box::new(move || {
                let done = PLACEMENTS.with(|pending| pending.borrow_mut().remove(&token));
                drop(completion);
                if let Some(done) = done { done(placement); }
            }));
        });
    }

    fn prepare_generated_manager_adoption(
        &self,
        operation_id: u64,
        plan: PreparedTabOpen<ViewId, super::super::super::host::RetainedSpareBinding>,
        view: ViewId,
        retained: super::super::super::host::RetainedSpareBinding,
        epoch: u64,
    ) -> Result<(
        super::super::super::host::GeneratedManagerAdoption,
        super::super::super::host::GeneratedManagerRegistration,
        main_core::shell::tab_manager::GeneratedRemovalPreview<ViewId, super::super::super::host::RetainedSpareBinding>,
        std::rc::Rc<RefCell<Option<String>>>,
        Box<dyn FnOnce() -> Option<PreparedTabOpen<ViewId, super::super::super::host::RetainedSpareBinding>>>,
    ), (PreparedTabOpen<ViewId, super::super::super::host::RetainedSpareBinding>, String)> {
        let original = plan.reserved_binding().map(|binding| binding.browser.owner.clone());
        let prepared = {
            let mut inner = self.inner.borrow_mut();
            let super::super::super::state::Inner { manager, bridge, .. } = &mut *inner;
            match manager.as_ref() {
                Some(manager) => manager.prepare_editor_adoption(bridge, plan, view)
                    .map_err(|(plan, error)| (plan, error.0)),
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
                Some(manager) => manager.preview_prepared_generated_adoption(&prepared, &commit, &retained),
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
        let recover = Box::new(move || prepared.borrow_mut().take().map(|prepared| prepared.into_plan()));
        let binding = retained.browser.clone();
        let old_binding = retained.clone();
        let mut registered_binding = retained.clone();
        let registration_binding = binding.clone();
        let tab_id = std::rc::Rc::new(RefCell::new(None));
        let admitted_id = tab_id.clone();
        let registered_id = tab_id.clone();
        let registration_state = self.rc();
        let state = self.rc();
        let adopt = Box::new(move |transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
            owner: &shell_contract::ipc::NativeBrowserOwner,
            actor: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>| {
            let matching_owner = match &original {
                Some(original) => transition.matches_owner(original) && transition.proposes_owner(owner),
                None => transition.matches_owner(owner),
            };
            if state.manager_epoch.get() != epoch || state.creation_shutdown.get()
                || !transition.matches(&binding.reservation) || !owner.same_owner(&binding.owner)
                || !matching_owner {
                return Err("The generated editor adoption no longer matches its reservation".into());
            }
            let mut pending = state.pending_owned.borrow_mut();
            let entry = pending.get_mut(&operation_id).ok_or("The generated tab operation was cancelled")?;
            if entry.epoch != epoch || !matches!(&entry.step, Step::GeneratedFailed { plan: None, view: pending_view } if *pending_view == view) {
                return Err("The generated tab operation changed before adoption".into());
            }
            let mut inner = state.inner.borrow_mut();
            let manager = inner.manager.as_mut().ok_or("The shell window is unavailable")?;
            {
                let prepared = adoption_prepared.borrow();
                manager.validate_editor_adoption_commit(prepared.as_ref().ok_or("The generated manager adoption was already consumed")?, &commit)?;
            }
            let mut commit = Some(commit);
            let mut retained = Some(retained);
            actor(&mut || {
                let prepared = adoption_prepared.borrow_mut().take().ok_or("The generated manager adoption was already consumed")?;
                let commit = commit.take().ok_or("The generated manager commit was already consumed")?;
                let retained = retained.take().ok_or("The generated browser binding was already consumed")?;
                let loaded_id = loaded_id.take().ok_or("The generated tab identity was already consumed")?;
                *admitted_id.borrow_mut() = Some(manager.commit_validated_editor_adoption(prepared, retained, commit));
                entry.step = Step::GeneratedLoaded { id: loaded_id, view };
                Ok(())
            })
        });
        let register = Box::new(move |owner: &NativeRequestOwner,
            transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
            register: &mut dyn FnMut() -> Result<(), String>| {
            if registration_state.manager_epoch.get() != epoch || registration_state.creation_shutdown.get()
                || owner.view != view || !transition.matches(&registration_binding.reservation)
                || !transition.matches_owner(&registration_binding.owner)
                || !owner.context.renderer().is_some_and(|created| registration_binding.reservation.matches(view, created)) {
                return Err("The generated editor registration no longer matches its reservation".into());
            }
            let id = registered_id.borrow();
            let id = id.as_deref().ok_or("The generated editor was not adopted")?;
            registered_binding.browser.registration = super::super::super::host::NativeBrowserRegistration::Registered(owner.clone());
            let mut inner = registration_state.inner.borrow_mut();
            let manager = inner.manager.as_mut().ok_or("The shell window is unavailable")?;
            manager.replace_tab_binding_admitted(id, &view, &old_binding, registered_binding, register)
        });
        Ok((adopt, register, preview, tab_id, recover))
    }

    fn start_generated_open(&self, request: EditorOpen, placement: Option<Option<NewDocumentPlacement>>, source: NativeRequestOwner, mut participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken, epoch: u64, done: GeneratedDone) {
        if epoch != self.manager_epoch.get() || cancel.is_cancelled() || pending.is_cancelled() { return done(Err("The initialized tab operation is no longer active".into())); }
        let id = match self.owned_id() { Ok(id) => id, Err(error) => return done(Err(error)) };
        let Some(mut plan) = self.inner.borrow_mut().manager_call(|manager, host| manager.prepare_editor_open(host, request)) else {
            return done(Err("The shell window is unavailable".into()));
        };
        let seed = plan.take_seed();
        let spare = if let Some(view) = plan.reserved_view().copied() {
            let captured = (|| {
                let binding = plan.reserved_binding().cloned().ok_or("The reserved spare binding is unavailable")?;
                let owner = match &binding.browser.registration {
                    super::super::super::host::NativeBrowserRegistration::Registered(owner) if owner.view == view => owner.clone(),
                    _ => return Err("The reserved spare is not registered".to_owned()),
                };
                let generation = owner.context.renderer().cloned().ok_or("The reserved renderer is unavailable")?;
                Ok::<_, String>((view, generation, owner, binding))
            })();
            match captured {
                Ok((view, generation, owner, binding)) => { participants.push(owner); Some((view, generation, binding)) }
                Err(error) => { self.cancel_owned_step(Step::New(plan)); return done(Err(error)); }
            }
        } else { None };
        let cleanup_shell = (|| {
            let shell = self.shell_window().ok_or("The shell window is unavailable")?;
            let generation = self.services.views.renderer_generation(shell.view).ok_or("The shell renderer is unavailable")?;
            let effects = self.services.hosts.get::<OwnedEffectsHost>().ok_or("Owned cleanup is unavailable")?;
            effects.capture_owner(shell.view, &generation).map(|owner| (shell, owner))
        })();
        let cleanup_shell = match cleanup_shell {
            Ok(captured) => captured,
            Err(error) => { self.cancel_owned_step(Step::New(plan)); return done(Err(error)); }
        };
        let captured_tabs = (|| -> Result<Vec<GeneratedCapturedTab>, String> {
            let tabs = {
                let inner = self.inner.borrow();
                let manager = inner.manager.as_ref().ok_or("The shell window is unavailable")?;
                manager.list().into_iter().map(|tab| {
                    let view = manager.web_contents_for_tab(&tab.id);
                    let unloaded = manager.is_tab_unloaded(&tab.id);
                    (tab.id, view, unloaded)
                }).collect::<Vec<_>>()
            };
            let effects = self.services.hosts.get::<OwnedEffectsHost>().ok_or("Owned cleanup is unavailable")?;
            tabs.into_iter().map(|(id, view, unloaded)| match view {
                Some(view) => {
                    let generation = self.services.views.renderer_generation(view).ok_or("A surviving tab renderer is unavailable")?;
                    Ok(GeneratedCapturedTab::Loaded { id, owner: effects.capture_owner(view, &generation)? })
                }
                None if unloaded => Ok(GeneratedCapturedTab::Unloaded { id }),
                None => Ok(GeneratedCapturedTab::Viewless { id }),
            }).collect()
        })();
        let tabs = match captured_tabs {
            Ok(tabs) => tabs,
            Err(error) => { self.cancel_owned_step(Step::New(plan)); return done(Err(error)); }
        };
        let rollback: super::super::super::host::EditorSeedRollbackStore = std::rc::Rc::default();
        let generated_cleanup = GeneratedCleanupAuthority {
            source: source.clone(), shell: cleanup_shell.1, shell_window: cleanup_shell.0,
            manager_epoch: epoch, cleanup: self.hooks.cleanup_lifetime(), participants: participants.clone(), survivors: tabs,
            preview: None, reservation: None, retirement: std::rc::Rc::new(RefCell::new(None)),
            cancel: cancel.clone(), pending: pending.clone(), rollback: rollback.clone(),
        };
        let prepared = match plan.creation_spec() {
            Some(spec) => self.hooks.prepare_generated_view_owned(
                &spec, source.clone(), participants.clone(), cancel.clone(), pending.clone(),
                seed, placement, rollback.clone()),
            None => match spare {
                Some((view, generation, binding)) => self.hooks.prepare_initialized_spare_owned(
                    view, &generation, binding, source.clone(), participants.clone(), cancel.clone(),
                    pending.clone(), seed, placement, rollback.clone()),
                None => Err(TabError("The reserved initialized renderer is unavailable".into())),
            },
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let state = self.rc();
                let reply=std::rc::Rc::new(RefCell::new(Some(done)));
                let failed=reply.clone(); let message=error.0.clone();
                self.retain_editor_cleanup(None,None,rollback,false,Box::new(move |failure| {
                    if let Some(done)=failed.borrow_mut().take() { done(Err(format!("{message}; cleanup failed: {failure}"))); }
                }),Box::new(move || {
                    state.cancel_owned_step(Step::New(plan));
                    if let Some(done)=reply.borrow_mut().take() { done(Err(error.0)); }
                }));
                return;
            }
        };
        if let Err(error)=self.begin_creation(id) {
            let state = self.rc();
            let reply=std::rc::Rc::new(RefCell::new(Some(done)));
            let failed=reply.clone(); let message=error.clone();
            self.retain_editor_cleanup(None,None,rollback,false,Box::new(move |failure| {
                if let Some(done)=failed.borrow_mut().take() { done(Err(format!("{message}; cleanup failed: {failure}"))); }
            }),Box::new(move || {
                state.cancel_owned_step(Step::New(plan));
                if let Some(done)=reply.borrow_mut().take() { done(Err(error)); }
            }));
            return;
        }
        self.pending_owned.borrow_mut().insert(id, PendingOwnedOpen { epoch, cancel: cancel.clone(), generation, step: Step::GeneratedNew(plan), done: Completion::Generated(done), generated_cleanup: Some(generated_cleanup), diagram: None });
        let state=self.rc();
        construct(source,cancel,Box::new(move |result| match result {
            Ok(ready)=>{
                if !state.pending_owned.borrow().contains_key(&id) {
                    let completed = state.clone();
                    state.retain_editor_cleanup(Some(ready.cleanup),None,rollback,true,
                        Box::new(|error|eprintln!("[shell] orphaned generated cleanup failed: {error}")),
                        Box::new(move || {
                            eprintln!("[shell] discarded orphaned generated editor {id}");
                            completed.finish_creation(id);
                        }));
                    return;
                }
                state.finish_owned_open(id,Ok(NativeOpened::Generated(ready)));
                state.finish_creation(id);
            }
            Err(failure)=>{
                let main_core::shell::tab_manager::ViewCreationFailure { error, cleanup } = failure;
                let (cleanup,proof) = match cleanup {
                    main_core::shell::tab_manager::ViewCreationCleanupState::Complete => (None,None),
                    main_core::shell::tab_manager::ViewCreationCleanupState::Acknowledged(proof) => (None,Some(proof)),
                    main_core::shell::tab_manager::ViewCreationCleanupState::Pending(cleanup) => (Some(cleanup),None),
                };
                let completed=state.clone();
                let failed=state.clone(); let original=error.0.clone();
                let retirement = state.pending_owned.borrow().get(&id)
                    .and_then(|entry| entry.generated_cleanup.as_ref().map(|authority| authority.retirement.clone()));
                let action = std::rc::Rc::new(move |proof: Option<shell_contract::ipc::NativeBrowserRetirement>, done| {
                    if let Some(retirement) = &retirement { *retirement.borrow_mut() = proof.clone(); }
                    super::receipt_retirement::rollback_editor_receipts(rollback.clone(), proof, true, done);
                });
                state.retain_creation_cleanup_with_rollback(cleanup,proof,Some(action),Box::new(move |failure| {
                    failed.notify_pending_creation_error(id,format!("{original}; cleanup failed: {failure}"));
                }),Box::new(move || {
                    completed.finish_owned_open(id,Err(error.0));
                    completed.finish_creation(id);
                }));
            }
        }));
    }

    fn commit_generated_open(&self, id: u64, view: ViewId, created: &RendererGeneration, seed: Option<EditorSeed>, placement: Option<Option<NewDocumentPlacement>>, source: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, request_pending: &CancelToken, reservation: &shell_contract::ipc::NativeClosureReservation, rollback: &super::super::super::host::EditorSeedRollbackStore) -> Result<(), TabError> {
        let Some(mut pending) = self.pending_owned.borrow_mut().remove(&id) else { return Err(TabError("The generated tab operation was cancelled".into())); };
        let PendingOwnedOpen { epoch, generation, step, .. } = &pending;
        if *epoch != self.manager_epoch.get() || !generation.same_generation(created) || !created.is_active() || !matches!(step, Step::GeneratedNew(_)) {
            self.pending_owned.borrow_mut().insert(id, pending);
            return Err(TabError("The generated target changed before initialization".into()));
        }
        let Step::GeneratedNew(plan) = pending.step else { unreachable!("checked generated phase") };
        if let Err(error) = self.hooks.commit_generated_seed_owned(view, seed, placement, source, participants, cancel, request_pending, created, reservation, rollback) {
            pending.step = Step::GeneratedFailed { plan: Some(plan), view };
            self.pending_owned.borrow_mut().insert(id, pending);
            return Err(error);
        }
        let created_view = if plan.reserved_view().is_some() { None } else { Some(view) };
        let resumed = self.inner.borrow_mut().manager_call(|manager, host| manager.resume_editor_open(host, plan, Ok(created_view)))
            .unwrap_or_else(|| Err(TabError("The shell window is unavailable".into())));
        match resumed {
            Ok(tab_id) => {
                let preview = self.inner.borrow().manager.as_ref()
                    .ok_or_else(|| "The shell window is unavailable".to_owned())
                    .and_then(|manager| manager.preview_closed_generated_owned(&tab_id, &view));
                pending.step = Step::GeneratedLoaded { id: tab_id, view };
                let result = match (pending.generated_cleanup.as_mut(), preview) {
                    (Some(authority), Ok(preview)) => {
                        authority.reservation = Some(reservation.clone());
                        authority.preview = Some(preview);
                        Ok(())
                    }
                    (None, _) => Err(TabError("The generated cleanup authority is unavailable".into())),
                    (_, Err(error)) => Err(TabError(error)),
                };
                self.pending_owned.borrow_mut().insert(id, pending);
                result
            }
            Err(error) => {
                pending.step = Step::GeneratedFailed { plan: None, view };
                self.pending_owned.borrow_mut().insert(id, pending);
                Err(error)
            }
        }
    }

    pub(super) fn finish_generated_open(&self, epoch: u64, generation: RendererGeneration, step: Step, result: Result<NativeOpened, String>, done: GeneratedDone) {
        if epoch != self.manager_epoch.get() {
            self.close_owned_result(result);
            self.cancel_owned_step(step);
            return done(Err("The shell window changed while opening the generated document".into()));
        }
        match (step, result) {
            (Step::GeneratedLoaded { id, view }, Ok(NativeOpened::Generated(commit)))
                if commit.view() == view && commit.created().same_generation(&generation) => {
                let Some(effects) = self.services.hosts.get::<OwnedEffectsHost>().cloned() else {
                    self.hooks.rollback_generated_seed_owned(view, &generation);
                    self.close_owned_result(Ok(NativeOpened::Generated(commit)));
                    return done(Err("Owned target completion effects are unavailable".into()));
                };
                let receipt = CommittedGeneratedTab { tab_id: id, target: commit.into_target(), created: generation, effects };
                if let Err(error) = receipt.validate_target() {
                    self.hooks.rollback_generated_seed_owned(view, receipt.created());
                    self.cancel_owned_step(Step::GeneratedLoaded { id: receipt.tab_id, view });
                    done(Err(error));
                } else { done(Ok(receipt)); }
            }
            (step, result) => {
                let error = match result {
                    Err(error) => error,
                    Ok(other) => { self.close_owned_result(Ok(other)); "The generated editor returned a mismatched completion".into() }
                };
                if let Step::GeneratedLoaded { view, .. } | Step::GeneratedFailed { view, .. } = &step {
                    self.hooks.rollback_generated_seed_owned(*view, &generation);
                }
                self.cancel_owned_step(step);
                done(Err(error));
            }
        }
    }
}
