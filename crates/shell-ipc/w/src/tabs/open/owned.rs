use std::cell::RefCell;
use std::collections::HashMap;

use main_core::shell::tab_manager::{EditorOpen, PreparedTabLoad, PreparedTabOpen, TabKind};
use shell_contract::{
    ViewId,
    ipc::NativeRequestOwner,
    runtime::{CancelToken, RendererGeneration},
};

use super::{DIAGRAM_CHECKING, TabsState, document_kind};

mod adopted_lifetime;
mod committed;
mod creation_cleanup;
mod generated;
mod generated_cleanup;
mod help;
mod native_command;
mod pending_output;
mod receipt_retirement;
mod startup_native;
mod saved_input;
mod tab_load;
pub(crate) use adopted_lifetime::AdoptedEditorLifetime;
pub use adopted_lifetime::{
    AdoptedLifetimeFailure, AdoptedLifetimeHandle, CallerOwnedAdoptedLifetime,
    RetainedAdoptedLifetime,
};
pub use committed::PreparedGeneratedOperation;
pub(crate) use creation_cleanup::OwnedCreationCleanup;
pub use generated::{CommittedGeneratedTab, GeneratedEditorOpen};
pub use receipt_retirement::{retire_adopted_editor_receipts, rollback_editor_receipts};
type Opened = (String, ViewId, RendererGeneration);
type Done = Box<dyn FnOnce(Result<Opened, String>)>;
type Routed = Box<dyn FnOnce(Result<Option<ViewId>, String>)>;

fn editor_request_for_path(kind: TabKind, path: &str) -> Option<EditorOpen> {
    let path = path.to_owned();
    Some(match kind {
        TabKind::Docs => EditorOpen::Docs {
            path: Some(path),
            options: None,
        },
        TabKind::Sheets => EditorOpen::Sheets {
            path: Some(path),
            new_blank: false,
        },
        TabKind::Slides => EditorOpen::Slides { path: Some(path) },
        TabKind::Pdf => EditorOpen::Pdf { path },
        TabKind::Markdown => EditorOpen::Markdown { path: Some(path) },
        TabKind::Html => EditorOpen::Html { path: Some(path) },
        TabKind::Mindmap => EditorOpen::Mindmap { path: Some(path) },
        TabKind::Hwp => EditorOpen::Hwp {
            path: Some(path),
            copy: None,
        },
        TabKind::Epub => EditorOpen::Epub { path },
        TabKind::Diagram => EditorOpen::Diagram { path: Some(path) },
        TabKind::Home | TabKind::Help => return None,
    })
}

fn surface_for_kind(kind: TabKind) -> Option<shell_contract::Surface> {
    Some(match kind {
        TabKind::Docs => shell_contract::Surface::Docs,
        TabKind::Sheets => shell_contract::Surface::Sheets,
        TabKind::Slides => shell_contract::Surface::Slides,
        TabKind::Pdf => shell_contract::Surface::Pdf,
        TabKind::Markdown => shell_contract::Surface::Markdown,
        TabKind::Html => shell_contract::Surface::Html,
        TabKind::Mindmap => shell_contract::Surface::Mindmap,
        TabKind::Hwp => shell_contract::Surface::Hwp,
        TabKind::Epub => shell_contract::Surface::Epub,
        TabKind::Diagram => shell_contract::Surface::Diagram,
        TabKind::Home | TabKind::Help => return None,
    })
}

pub fn native_open_surface(path: &str) -> shell_contract::Surface {
    document_kind(path)
        .and_then(surface_for_kind)
        .unwrap_or(shell_contract::Surface::Shell)
}

enum Completion {
    Plain(Done),
    Generated(generated::GeneratedDone),
    Saved(saved_input::SavedDone),
    Replied,
}

impl Completion {
    fn fail(self, error: String) {
        match self {
            Self::Plain(done) => done(Err(error)),
            Self::Generated(done) => done(Err(error)),
            Self::Saved(done) => done(Err(error)),
            Self::Replied => {}
        }
    }
}

thread_local! {
    #[allow(clippy::type_complexity)]
    static DIAGRAM_WAITERS: RefCell<HashMap<String, Vec<Box<dyn FnOnce()>>>> = RefCell::new(HashMap::new());
}

pub(super) struct DiagramCheckGuard(String);
impl DiagramCheckGuard {
    pub(super) fn new(key: String) -> Self {
        Self(key)
    }
}
impl Drop for DiagramCheckGuard {
    fn drop(&mut self) {
        DIAGRAM_CHECKING.with(|checking| checking.borrow_mut().remove(&self.0));
        let waiting = DIAGRAM_WAITERS.with(|waiting| waiting.borrow_mut().remove(&self.0));
        if let Some(waiting) = waiting {
            for resume in waiting {
                resume();
            }
        }
    }
}

pub(super) enum SurvivorHydration {
    Ready {
        id: String,
        owner: NativeRequestOwner,
        generation: RendererGeneration,
    },
    AdoptedPending(Box<AdoptedSurvivorPending>),
}

pub(super) struct AdoptedSurvivorPending {
    pub id: String,
    pub owner: NativeRequestOwner,
    pub generation: RendererGeneration,
    pub closure: super::super::host::ViewClosureBinding,
    pub attempt: std::rc::Rc<dyn super::super::activation::RecoveryAttempt>,
    pub error: String,
    pub lifetime: Option<AdoptedLifetimeFailure>,
    pub handle: Option<AdoptedLifetimeHandle>,
    pub lifetime_secured: bool,
    pub attempt_secured: bool,
}

impl AdoptedSurvivorPending {
    pub fn retry(&mut self) -> Result<(), String> {
        if !self.lifetime_secured
            && let Some(failure) = self.lifetime.take()
        {
            match failure {
                AdoptedLifetimeFailure::CallerOwned(failure) => {
                    if !self.generation.is_active() {
                        self.lifetime = Some(AdoptedLifetimeFailure::CallerOwned(failure));
                        self.error = "The adopted survivor renderer is no longer active".into();
                        return Err(self.error.clone());
                    }
                    if let Err(error) = self
                        .closure
                        .reservation
                        .with_owner(&self.closure.owner, |_| Ok(()))
                    {
                        self.lifetime = Some(AdoptedLifetimeFailure::CallerOwned(failure));
                        self.error = error;
                        return Err(self.error.clone());
                    }
                    match failure.retry() {
                        Ok(handle) => {
                            self.handle = Some(handle);
                            self.lifetime_secured = true;
                        }
                        Err(failure) => {
                            self.error = failure.message().to_owned();
                            self.lifetime = Some(failure);
                            return Err(self.error.clone());
                        }
                    }
                }
                AdoptedLifetimeFailure::Retained(failure) => {
                    let result = failure.handle().retry();
                    self.lifetime = Some(AdoptedLifetimeFailure::Retained(failure));
                    if let Err(error) = result {
                        self.error = error;
                    }
                    return Err(self.error.clone());
                }
            }
        }
        let handle = self
            .handle
            .as_ref()
            .ok_or("The survivor lifetime handle is unavailable")?;
        if !handle.is_unclosed()? {
            return Err("The adopted survivor is retiring".into());
        }
        if !self.attempt_secured {
            if let Err(error) = self
                .closure
                .reservation
                .with_owner(&self.closure.owner, |_| self.attempt.adopted())
            {
                self.error = error;
                return Err(self.error.clone());
            }
            self.attempt_secured = true;
        }
        Ok(())
    }
}

fn retain_lifetime_failure(
    state: &TabsState,
    failure: Option<AdoptedLifetimeFailure>,
    initial_handle: Option<AdoptedLifetimeHandle>,
    generation: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    recovery: Option<(
        std::rc::Rc<dyn super::super::activation::RecoveryAttempt>,
        super::super::activation::RecoveryCleanup,
    )>,
) {
    let retry = RefCell::new(failure);
    let handle = RefCell::new(initial_handle);
    let terminal_only = std::cell::Cell::new(false);
    let recovery_started = std::rc::Rc::new(std::cell::Cell::new(false));
    let waiter = std::rc::Rc::new(RefCell::new(None::<Box<dyn FnOnce(Result<(), String>)>>));
    let settled = std::rc::Rc::new(RefCell::new(None::<Result<(), String>>));
    let action = std::rc::Rc::new(move |_, done: Box<dyn FnOnce(Result<(), String>)>| {
        if let Some(result) = settled.borrow().clone() {
            return done(result);
        }
        if recovery_started.get() {
            *waiter.borrow_mut() = Some(done);
            if let Some((attempt, _)) = &recovery {
                attempt.retry_cleanup();
            }
            return;
        }
        let failure = retry.borrow_mut().take();
        if let Some(failure) = failure {
            match failure {
                AdoptedLifetimeFailure::CallerOwned(failure) => {
                    let valid = if generation.is_active() {
                        reservation.with_owner(&owner, |_| Ok(()))
                    } else {
                        Err("The adopted renderer is no longer active".into())
                    };
                    if let Err(error) = valid {
                        *retry.borrow_mut() = Some(AdoptedLifetimeFailure::CallerOwned(failure));
                        return done(Err(error));
                    }
                    match failure.retry() {
                        Ok(value) => *handle.borrow_mut() = Some(value),
                        Err(failure) => {
                            let error = failure.message().to_owned();
                            *retry.borrow_mut() = Some(failure);
                            return done(Err(error));
                        }
                    }
                }
                AdoptedLifetimeFailure::Retained(failure) => {
                    terminal_only.set(true);
                    *handle.borrow_mut() = Some(failure.into_handle());
                }
            }
        }
        let Some(handle) = handle.borrow().clone() else {
            return done(Err("The adopted lifetime handle is unavailable".into()));
        };
        if let Err(error) = handle.retry() {
            return done(Err(error));
        }
        if !terminal_only.get() && generation.is_active() && handle.is_unclosed() == Ok(true) {
            let result = reservation.with_owner(&owner, |_| match &recovery {
                Some((attempt, _)) => attempt.adopted(),
                None => Ok(()),
            });
            return done(result);
        }
        let recovery = recovery.clone();
        let started = recovery_started.clone();
        let waiting = waiter.clone();
        let terminal = settled.clone();
        handle.observe_terminal(Box::new(move |proof| match recovery {
            Some((attempt, _)) => {
                started.set(true);
                *waiting.borrow_mut() = Some(done);
                let failed = waiting.clone();
                let completed = waiting.clone();
                attempt.fail_after_retirement(
                    "The adopted editor retired before recovery completed".into(),
                    proof,
                    std::rc::Rc::new(move |error| {
                        if let Some(done) = failed.borrow_mut().take() {
                            done(Err(error));
                        }
                    }),
                    Box::new(move || {
                        *terminal.borrow_mut() = Some(Ok(()));
                        if let Some(done) = completed.borrow_mut().take() {
                            done(Ok(()));
                        }
                    }),
                );
            }
            None => done(Ok(())),
        }));
    });
    state.retain_creation_cleanup_with_rollback(
        None,
        None,
        Some(action),
        Box::new(|error| eprintln!("[shell] adopted lifetime remains pending: {error}")),
        Box::new(|| {}),
    );
}

impl TabsState {
    pub(crate) fn retain_failed_adopted_lifetime(
        &self,
        failure: AdoptedLifetimeFailure,
        generation: RendererGeneration,
        reservation: shell_contract::ipc::NativeClosureReservation,
        owner: shell_contract::ipc::NativeBrowserOwner,
    ) {
        retain_lifetime_failure(
            self,
            Some(failure),
            None,
            generation,
            reservation,
            owner,
            None,
        );
    }
}

struct PreparedSurvivorHydration {
    outcome: std::rc::Rc<RefCell<Option<SurvivorHydration>>>,
    pending: CancelToken,
    shell_owner: NativeRequestOwner,
    shell_window: super::super::host::ShellWindow,
}

enum Step {
    New(PreparedTabOpen<ViewId, super::super::host::RetainedSpareBinding>),
    Hydrate {
        plan: PreparedTabLoad,
        origin: NativeRequestOwner,
        startup: Option<Box<super::super::activation::StartupRecoveryConstraint>>,
        prepared: Option<PreparedSurvivorHydration>,
    },
    NativeHydrate {
        plan: PreparedTabLoad,
        command: shell_contract::ipc::NativeInvocationOperation,
        shell: NativeRequestOwner,
        /// Set for a hydration that adopts the view without activating it
        prepared: Option<PreparedSurvivorHydration>,
    },
    GeneratedLoaded {
        id: String,
        view: ViewId,
    },
    GeneratedFailed {
        plan: Option<PreparedTabOpen<ViewId, super::super::host::RetainedSpareBinding>>,
        view: ViewId,
    },
}

pub(super) struct GeneratedCleanupAuthority {
    pub operation: u64,
    pub source: NativeRequestOwner,
    pub shell: NativeRequestOwner,
    pub shell_window: super::super::host::ShellWindow,
    pub manager_epoch: u64,
    pub cleanup: CancelToken,
    pub participants: Vec<NativeRequestOwner>,
    pub preview: main_core::shell::tab_manager::GeneratedRemovalPreview<
        ViewId,
        super::super::host::RetainedSpareBinding,
    >,
    pub reservation: shell_contract::ipc::NativeClosureReservation,
    pub browser: shell_contract::ipc::NativeBrowserOwner,
    pub retirement: std::rc::Rc<RefCell<Option<shell_contract::ipc::NativeBrowserRetirement>>>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub rollback: super::super::host::EditorSeedRollbackStore,
}

pub(crate) struct PendingOwnedOpen {
    epoch: u64,
    cancel: CancelToken,
    generation: RendererGeneration,
    step: Step,
    done: Completion,
    generated_cleanup: Option<std::rc::Rc<generated_cleanup::GeneratedCleanupCoordinator>>,
    diagram: Option<DiagramCheckGuard>,
}

enum NativeOpened {
    Hydrated(super::super::activation::OwnedLoadedTabView),
    Generated(super::super::host::ViewCreationReady<super::super::host::GeneratedViewCommit>),
}
pub(super) struct PreparedSavedManagerAdoption {
    pub adopt: super::super::host::SavedManagerAdoption,
    pub register: super::super::host::SavedManagerRegistration,
    pub tab_id: std::rc::Rc<RefCell<Option<String>>>,
    pub preview: main_core::shell::tab_manager::GeneratedRemovalPreview<
        ViewId,
        super::super::host::RetainedSpareBinding,
    >,
    pub recover: Box<
        dyn FnOnce() -> Option<PreparedTabOpen<ViewId, super::super::host::RetainedSpareBinding>>,
    >,
}

impl TabsState {
    #[allow(clippy::type_complexity)]
    pub(super) fn prepare_saved_manager_adoption(
        &self,
        plan: PreparedTabOpen<ViewId, super::super::host::RetainedSpareBinding>,
        target: &shell_contract::ipc::NativeSavedTarget,
        retained: super::super::host::RetainedSpareBinding,
        epoch: u64,
    ) -> Result<
        PreparedSavedManagerAdoption,
        Box<(
            PreparedTabOpen<ViewId, super::super::host::RetainedSpareBinding>,
            String,
        )>,
    > {
        let binding = &retained.browser;
        let original = match (
            plan.reserved_view(),
            plan.reserved_binding(),
            &binding.registration,
        ) {
            (None, None, super::super::host::NativeBrowserRegistration::Unregistered) => None,
            (
                Some(view),
                Some(original),
                super::super::host::NativeBrowserRegistration::Registered(owner),
            ) if *view == target.view()
                && original
                    .browser
                    .reservation
                    .same_reservation(&binding.reservation)
                && matches!(&original.browser.registration, super::super::host::NativeBrowserRegistration::Registered(previous)
                        if previous.view == owner.view
                            && std::sync::Arc::ptr_eq(&previous.context.as_arc(), &owner.context.as_arc())
                            && previous.loss_token().zip(owner.loss_token()).is_some_and(|(a, b)| a.same_token(&b))) =>
            {
                Some(original.browser.owner.clone())
            }
            _ => {
                return Err(Box::new((
                    plan,
                    "The saved editor reservation does not match its manager plan".into(),
                )));
            }
        };
        if !target.reservation().same_reservation(&binding.reservation)
            || !target.browser().same_owner(&binding.owner)
            || !binding.reservation.matches(target.view(), target.created())
        {
            return Err(Box::new((
                plan,
                "The saved editor reservation does not match its manager plan".into(),
            )));
        }
        let prepared = {
            let mut inner = self.inner.borrow_mut();
            let super::super::state::Inner {
                manager, bridge, ..
            } = &mut *inner;
            match manager.as_ref() {
                Some(manager) => manager
                    .prepare_editor_adoption(bridge, plan, target.view())
                    .map_err(|failure| {
                        let (plan, error) = *failure;
                        Box::new((plan, error.0))
                    }),
                None => Err(Box::new((plan, "The shell window is unavailable".into()))),
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
            Err(error) => return Err(Box::new((prepared.into_plan(), error))),
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
            Err(error) => return Err(Box::new((prepared.into_plan(), error))),
        };
        let prepared = std::rc::Rc::new(RefCell::new(Some(prepared)));
        let adoption_prepared = prepared.clone();
        let recover = Box::new(move || {
            prepared
                .borrow_mut()
                .take()
                .map(|prepared| prepared.into_plan())
        });
        let expected = target.clone();
        let old_binding = retained.clone();
        let mut registered_binding = retained.clone();
        let registration_target = target.clone();
        let tab_id = std::rc::Rc::new(RefCell::new(None));
        let admitted_id = tab_id.clone();
        let registered_id = tab_id.clone();
        let registration_state = self.rc();
        let state = self.rc();
        let adopt = Box::new(
            move |target: &shell_contract::ipc::NativeSavedTarget,
                  transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
                  actor: &mut dyn FnMut(
                &mut dyn FnMut() -> Result<(), String>,
            ) -> Result<(), String>| {
                let matching_owner = match &original {
                    Some(original) => {
                        transition.matches_owner(original)
                            && transition.proposes_owner(target.browser())
                    }
                    None => transition.matches_owner(target.browser()),
                };
                if !target.same_target(&expected)
                    || !transition.matches(target.reservation())
                    || !matching_owner
                    || state.manager_epoch.get() != epoch
                    || state.creation_shutdown.get()
                {
                    return Err(
                        "The saved editor adoption no longer matches its reservation".into(),
                    );
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
                            .ok_or("The saved manager adoption was already consumed")?,
                        &commit,
                    )?;
                }
                let mut commit = Some(commit);
                let mut retained = Some(retained);
                actor(&mut || {
                    let prepared = adoption_prepared
                        .borrow_mut()
                        .take()
                        .ok_or("The saved manager adoption was already consumed")?;
                    let commit = commit
                        .take()
                        .ok_or("The saved manager commit was already consumed")?;
                    let retained = retained
                        .take()
                        .ok_or("The saved browser binding was already consumed")?;
                    *admitted_id.borrow_mut() =
                        Some(manager.commit_validated_editor_adoption(prepared, retained, commit));
                    Ok(())
                })
            },
        );
        let register = Box::new(
            move |target: &shell_contract::ipc::NativeSavedTarget,
                  owner: &NativeRequestOwner,
                  transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
                  register: &mut dyn FnMut() -> Result<(), String>| {
                if !target.same_target(&registration_target)
                    || !transition.matches(target.reservation())
                    || !transition.matches_owner(target.browser())
                    || owner.view != target.view()
                    || owner
                        .context
                        .renderer()
                        .is_none_or(|renderer| !renderer.same_generation(target.created()))
                    || registration_state.manager_epoch.get() != epoch
                    || registration_state.creation_shutdown.get()
                {
                    return Err(
                        "The saved editor registration no longer matches its reservation".into(),
                    );
                }
                let id = registered_id.borrow();
                let id = id.as_deref().ok_or("The saved editor was not adopted")?;
                registered_binding.browser.registration =
                    super::super::host::NativeBrowserRegistration::Registered(owner.clone());
                let mut inner = registration_state.inner.borrow_mut();
                let manager = inner
                    .manager
                    .as_mut()
                    .ok_or("The shell window is unavailable")?;
                manager.replace_tab_binding_admitted(
                    id,
                    &owner.view,
                    &old_binding,
                    registered_binding,
                    register,
                )
            },
        );
        Ok(PreparedSavedManagerAdoption {
            adopt,
            register,
            tab_id,
            recover,
            preview,
        })
    }

    pub(crate) fn owned_id(&self) -> Result<u64, String> {
        let id = self.next_owned.get();
        let next = id
            .checked_add(1)
            .ok_or("owned tab operation ids exhausted")?;
        self.next_owned.set(next);
        Ok(id)
    }

    pub fn open_editor_owned(
        &self,
        request: EditorOpen,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        done: Done,
    ) {
        self.open_editor_owned_at(request, owner, cancel, self.manager_epoch.get(), None, done);
    }

    fn open_editor_owned_at(
        &self,
        request: EditorOpen,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        epoch: u64,
        diagram: Option<DiagramCheckGuard>,
        done: Done,
    ) {
        if matches!(
            request,
            EditorOpen::Generate { .. } | EditorOpen::Conversion { .. }
        ) {
            return done(Err(
                "Generated documents require the committed initialization path".into(),
            ));
        }
        if epoch != self.manager_epoch.get() || cancel.is_cancelled() {
            done(Err("the owned tab operation is no longer active".into()));
            return;
        }
        let id = match self.owned_id() {
            Ok(id) => id,
            Err(error) => {
                done(Err(error));
                return;
            }
        };
        let reserved = {
            let mut state = self.inner.borrow_mut();
            state.manager_call(|manager, host| manager.prepare_editor_open(host, request))
        };
        let Some(mut plan) = reserved else {
            done(Err("the shell window is not available".into()));
            return;
        };
        let new_blank = plan.requires_sheets_new_blank();
        let (seed, docs_path, workbook, _) = plan.initialization_inputs();
        let rollback: super::super::host::EditorSeedRollbackStore = std::rc::Rc::default();
        let preparation = match plan.creation_spec() {
            Some(spec) => {
                let hooks = self.hooks.clone();
                let source = owner.clone();
                let request_cancel = cancel.clone();
                let initialize_rollback = rollback.clone();
                self.hooks.prepare_view_owned(
                    &spec,
                    rollback.clone(),
                    Box::new(move |view, created, reservation, browser_owner| {
                        hooks.commit_editor_seed_owned(
                            view,
                            seed,
                            docs_path,
                            workbook,
                            None,
                            &source,
                            &initialize_rollback,
                            created,
                            reservation,
                            browser_owner,
                            &request_cancel,
                            &request_cancel,
                        )
                    }),
                )
            }
            None => {
                let Some(view) = plan.reserved_view().copied() else {
                    self.cancel_owned_step(Step::New(plan));
                    return done(Err("the reserved editor view is unavailable".into()));
                };
                let Some(retained) = plan.reserved_binding().cloned() else {
                    self.cancel_owned_step(Step::New(plan));
                    return done(Err("the reserved editor binding is unavailable".into()));
                };
                let target = match &retained.browser.registration {
                    super::super::host::NativeBrowserRegistration::Registered(owner)
                        if owner.view == view =>
                    {
                        owner.clone()
                    }
                    _ => {
                        self.cancel_owned_step(Step::New(plan));
                        return done(Err("the reserved editor is not registered".into()));
                    }
                };
                let Some(generation) = target.context.renderer().cloned() else {
                    self.cancel_owned_step(Step::New(plan));
                    return done(Err("the reserved editor generation is unavailable".into()));
                };
                let staged_seed = match self.hooks.prepare_spare_editor_seed(
                    view, seed, docs_path, workbook, new_blank, &target, &retained, &rollback,
                ) {
                    Ok(staged) => staged,
                    Err(error) => {
                        self.cancel_owned_step(Step::New(plan));
                        return done(Err(error.0));
                    }
                };
                let binding = super::super::host::ViewClosureBinding {
                    reservation: retained.browser.reservation.clone(),
                    owner: retained.browser.owner.clone(),
                    producer: retained.browser.producer.clone(),
                    closure: retained.browser.closure.clone(),
                    registration: super::super::host::NativeBrowserRegistration::Registered(
                        target.clone(),
                    ),
                };
                self.pending_owned.borrow_mut().insert(
                    id,
                    PendingOwnedOpen {
                        epoch,
                        cancel: cancel.clone(),
                        generation,
                        step: Step::New(plan),
                        done: Completion::Plain(done),
                        generated_cleanup: None,
                        diagram,
                    },
                );
                self.finish_new_editor(
                    id,
                    super::super::activation::TabActivationAuthority::Interactive(owner),
                    target,
                    None,
                    binding,
                    rollback,
                    false,
                    Some(staged_seed),
                );
                return;
            }
        };
        let (generation, construct) = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                self.cancel_owned_step(Step::New(plan));
                return done(Err(error.0));
            }
        };
        if let Err(error) = self.begin_creation(id) {
            self.cancel_owned_step(Step::New(plan));
            return done(Err(error));
        }
        self.pending_owned.borrow_mut().insert(
            id,
            PendingOwnedOpen {
                epoch,
                cancel: cancel.clone(),
                generation,
                step: Step::New(plan),
                done: Completion::Plain(done),
                generated_cleanup: None,
                diagram,
            },
        );
        let state = self.rc();
        let source = owner.clone();
        construct(
            owner,
            cancel.clone(),
            cancel,
            Box::new(move |result| match result {
                Ok(ready) => {
                    let registration = super::super::host::NativeBrowserRegistration::Registered(
                        ready.view.clone(),
                    );
                    state.finish_new_editor(
                        id,
                        super::super::activation::TabActivationAuthority::Interactive(source),
                        ready.view,
                        Some(ready.cleanup),
                        super::super::host::ViewClosureBinding {
                            reservation: ready.reservation.reservation,
                            owner: ready.reservation.owner,
                            producer: ready.reservation.producer,
                            closure: ready.closure,
                            registration,
                        },
                        rollback,
                        true,
                        None,
                    )
                }
                Err(failure) => {
                    let completed = state.clone();
                    let main_core::shell::tab_manager::ViewCreationFailure { error, cleanup } =
                        failure;
                    let (cleanup, proof) = match cleanup {
                        main_core::shell::tab_manager::ViewCreationCleanupState::Complete => {
                            (None, None)
                        }
                        main_core::shell::tab_manager::ViewCreationCleanupState::Acknowledged(
                            proof,
                        ) => (None, Some(proof)),
                        main_core::shell::tab_manager::ViewCreationCleanupState::Pending(
                            cleanup,
                        ) => (Some(cleanup), None),
                    };
                    let failed = state.clone();
                    let original = error.0.clone();
                    state.retain_editor_cleanup(
                        cleanup,
                        proof,
                        rollback,
                        true,
                        Box::new(move |failure| {
                            failed.notify_pending_creation_error(
                                id,
                                format!("{original}; cleanup failed: {failure}"),
                            );
                        }),
                        Box::new(move || {
                            completed.finish_new_editor_failure(id, error.0);
                        }),
                    );
                    state.finish_creation(id);
                }
            }),
        );
    }

    fn finish_new_editor_failure(&self, id: u64, error: String) {
        let aborted = self.owned_abort.borrow_mut().remove(&id);
        let pending = self.pending_owned.borrow_mut().remove(&id);
        if let Some(pending) = pending {
            self.cancel_owned_step(pending.step);
            pending.done.fail(aborted.unwrap_or(error));
            drop(pending.diagram);
        }
    }

    fn notify_pending_creation_error(&self, id: u64, error: String) {
        let completion = {
            let mut pending = self.pending_owned.borrow_mut();
            pending
                .get_mut(&id)
                .map(|entry| std::mem::replace(&mut entry.done, Completion::Replied))
        };
        if let Some(completion) = completion {
            completion.fail(error);
        }
    }

    pub(crate) fn retain_editor_cleanup(
        &self,
        cleanup: Option<super::super::host::ViewCreationCleanup>,
        proof: Option<shell_contract::ipc::NativeBrowserRetirement>,
        rollback: super::super::host::EditorSeedRollbackStore,
        require_proof: bool,
        failure: Box<dyn FnMut(&str)>,
        done: Box<dyn FnOnce()>,
    ) {
        let action = std::rc::Rc::new(move |proof, done| {
            receipt_retirement::rollback_editor_receipts(
                rollback.clone(),
                proof,
                require_proof,
                done,
            );
        });
        self.retain_creation_cleanup_with_rollback(cleanup, proof, Some(action), failure, done);
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_new_editor(
        &self,
        id: u64,
        plan: PreparedTabOpen<ViewId, super::super::host::RetainedSpareBinding>,
        cleanup: Option<super::super::host::ViewCreationCleanup>,
        rollback: super::super::host::EditorSeedRollbackStore,
        created: bool,
        error: String,
        done: Done,
        diagram: Option<DiagramCheckGuard>,
    ) {
        let state = self.rc();
        let reply = std::rc::Rc::new(RefCell::new(Some(done)));
        let failed = reply.clone();
        let failure_message = error.clone();
        let finish = Box::new(move || {
            state.cancel_owned_step(Step::New(plan));
            if let Some(done) = reply.borrow_mut().take() {
                done(Err(error));
            }
            drop(diagram);
        });
        self.retain_editor_cleanup(
            cleanup,
            None,
            rollback,
            created,
            Box::new(move |failure| {
                if let Some(done) = failed.borrow_mut().take() {
                    done(Err(format!("{failure_message}; cleanup failed: {failure}")));
                }
            }),
            finish,
        );
        if created {
            self.finish_creation(id);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_new_editor(
        &self,
        id: u64,
        authority: super::super::activation::TabActivationAuthority,
        target: NativeRequestOwner,
        cleanup: Option<super::super::host::ViewCreationCleanup>,
        binding: super::super::host::ViewClosureBinding,
        rollback: super::super::host::EditorSeedRollbackStore,
        created: bool,
        spare_seed: Option<Box<dyn super::super::host::SpareSeedPlan>>,
    ) {
        let pending = self.pending_owned.borrow_mut().remove(&id);
        let Some(pending) = pending else {
            self.retain_editor_cleanup(
                cleanup,
                None,
                rollback,
                created,
                Box::new(|error| eprintln!("[shell] orphaned editor cleanup failed: {error}")),
                Box::new(move || eprintln!("[shell] discarded orphaned editor operation {id}")),
            );
            if created {
                self.finish_creation(id);
            }
            return;
        };
        let PendingOwnedOpen {
            epoch,
            cancel,
            generation,
            step,
            done,
            diagram,
            ..
        } = pending;
        let (plan, done) = match (step, done) {
            (Step::New(plan), Completion::Plain(done)) => (plan, done),
            (step, completion) => {
                self.cancel_owned_step(step);
                let completion = std::rc::Rc::new(RefCell::new(Some(completion)));
                let failed = completion.clone();
                self.retain_editor_cleanup(
                    cleanup,
                    None,
                    rollback,
                    created,
                    Box::new(move |error| {
                        if let Some(completion) = failed.borrow_mut().take() {
                            completion.fail(format!(
                                "unexpected editor completion; cleanup failed: {error}"
                            ));
                        }
                    }),
                    Box::new(move || {
                        if let Some(completion) = completion.borrow_mut().take() {
                            completion.fail("unexpected editor completion".into());
                        }
                    }),
                );
                if created {
                    self.finish_creation(id);
                }
                return;
            }
        };
        let aborted = self.owned_abort.borrow_mut().remove(&id);
        let invalid = aborted.is_some()
            || epoch != self.manager_epoch.get()
            || cancel.is_cancelled()
            || target
                .context
                .renderer()
                .is_none_or(|actual| !actual.same_generation(&generation));
        if invalid {
            return self.fail_new_editor(
                id,
                plan,
                cleanup,
                rollback,
                created,
                aborted.unwrap_or_else(|| "The editor reservation is no longer active".into()),
                done,
                diagram,
            );
        }
        if !binding.reservation.matches(target.view, &generation) {
            return self.fail_new_editor(
                id,
                plan,
                cleanup,
                rollback,
                created,
                "The editor closure reservation does not match its view".into(),
                done,
                diagram,
            );
        }
        let prepared = {
            let mut inner = self.inner.borrow_mut();
            let super::super::state::Inner {
                manager, bridge, ..
            } = &mut *inner;
            match manager.as_ref() {
                Some(manager) => manager.prepare_editor_adoption(bridge, plan, target.view),
                None => Err(Box::new((
                    plan,
                    main_core::shell::tab_manager::TabError(
                        "The shell window is unavailable".into(),
                    ),
                ))),
            }
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(failure) => {
                let (plan, error) = *failure;
                return self
                    .fail_new_editor(id, plan, cleanup, rollback, created, error.0, done, diagram);
            }
        };
        let Some(shell) = self.shell_window() else {
            return self.fail_new_editor(
                id,
                prepared.into_plan(),
                cleanup,
                rollback,
                created,
                "The shell window is unavailable".into(),
                done,
                diagram,
            );
        };
        let state = self.rc();
        let authority_for_gate = authority.clone();
        let target_for_gate = target.clone();
        let request_cancel = cancel.clone();
        self.hooks.adopt_recovery_view_owned(
            shell,
            authority.clone(),
            target.clone(),
            Vec::new(),
            cancel.clone(),
            cancel,
            Box::new(move |result| {
                if let Err(error) = result {
                    return state.fail_new_editor(
                        id,
                        prepared.into_plan(),
                        cleanup,
                        rollback,
                        created,
                        error,
                        done,
                        diagram,
                    );
                }
                let Some(effects) = state
                    .services
                    .hosts
                    .get::<shell_contract::hosts::OwnedEffectsHost>()
                    .cloned()
                else {
                    return state.fail_new_editor(
                        id,
                        prepared.into_plan(),
                        cleanup,
                        rollback,
                        created,
                        "Owned editor admission is unavailable".into(),
                        done,
                        diagram,
                    );
                };
                let old_binding = prepared.reserved_binding().cloned();
                let actor = old_binding
                    .as_ref()
                    .and_then(|binding| binding.spare_actor.clone());
                if old_binding.is_some() && (actor.is_none() || spare_seed.is_none()) {
                    return state.fail_new_editor(
                        id,
                        prepared.into_plan(),
                        cleanup,
                        rollback,
                        created,
                        "The spare transition is unavailable".into(),
                        done,
                        diagram,
                    );
                }
                let mut transfer = match actor
                    .as_ref()
                    .map(|actor| actor.prepare_transfer())
                    .transpose()
                {
                    Ok(transfer) => transfer,
                    Err(error) => {
                        return state.fail_new_editor(
                            id,
                            prepared.into_plan(),
                            cleanup,
                            rollback,
                            created,
                            error,
                            done,
                            diagram,
                        );
                    }
                };
                let successor_actor = transfer.as_ref().map(|plan| plan.successor());
                let successor_receipt = transfer.as_ref().map(|plan| plan.successor_receipt());
                if successor_receipt.is_some() && rollback.borrow_mut().try_reserve(1).is_err() {
                    return state.fail_new_editor(
                        id,
                        prepared.into_plan(),
                        cleanup,
                        rollback,
                        created,
                        "The editor receipt cannot be reserved".into(),
                        done,
                        diagram,
                    );
                }
                let commit = {
                    let mut inner = state.inner.borrow_mut();
                    match inner.manager.as_mut() {
                        Some(manager) => manager.prepare_editor_adoption_commit(&prepared),
                        None => Err("The shell window is unavailable".into()),
                    }
                };
                let commit = match commit {
                    Ok(commit) => commit,
                    Err(error) => {
                        return state.fail_new_editor(
                            id,
                            prepared.into_plan(),
                            cleanup,
                            rollback,
                            created,
                            error,
                            done,
                            diagram,
                        );
                    }
                };
                let mut prepared = Some(prepared);
                let mut commit = Some(commit);
                let mut tab = None;
                let mut spare_seed = spare_seed;
                let mut retained = super::super::host::RetainedSpareBinding::new(
                    super::super::host::NativeBrowserBinding {
                        reservation: binding.reservation.clone(),
                        owner: binding.owner.clone(),
                        producer: binding.producer.clone(),
                        closure: binding.closure.clone(),
                        registration: binding.registration.clone(),
                    },
                    rollback.clone(),
                    successor_actor,
                );
                let mut binding = binding;
                let mut superseded = None;
                let reservation = binding.reservation.clone();
                let original_owner = binding.owner.clone();
                let mut replacement = if transfer.is_some() {
                    match reservation.prepare_replacement(&original_owner) {
                        Ok(plan) => Some(plan),
                        Err(error) => {
                            if let Some(prepared) = prepared.take() {
                                return state.fail_new_editor(
                                    id,
                                    prepared.into_plan(),
                                    cleanup,
                                    rollback,
                                    created,
                                    error,
                                    done,
                                    diagram,
                                );
                            }
                            return done(Err(error));
                        }
                    }
                } else {
                    None
                };
                let admitted = super::super::activation::driver::admit(
                    &effects,
                    &authority_for_gate,
                    std::slice::from_ref(&target_for_gate),
                    &request_cancel,
                    &request_cancel,
                    || {
                        if state.creation_shutdown.get() || epoch != state.manager_epoch.get() {
                            return Err("The shell window changed before editor adoption".into());
                        }
                        let mut commit_manager =
                            |successor: &shell_contract::ipc::NativeBrowserOwner| {
                                let mut inner = state.inner.borrow_mut();
                                let manager = inner
                                    .manager
                                    .as_mut()
                                    .ok_or("The shell window is unavailable")?;
                                manager.validate_editor_adoption_commit(
                                    prepared
                                        .as_ref()
                                        .ok_or("The editor adoption was already consumed")?,
                                    commit
                                        .as_ref()
                                        .ok_or("The editor commit was already consumed")?,
                                )?;
                                retained.browser.owner = successor.clone();
                                if let Some(receipt) = successor_receipt.as_ref() {
                                    rollback.borrow_mut().push(receipt.clone());
                                }
                                let adoption = prepared
                                    .take()
                                    .ok_or("The editor adoption was already consumed")?;
                                let commit = commit
                                    .take()
                                    .ok_or("The editor commit was already consumed")?;
                                tab = Some(manager.commit_validated_editor_adoption(
                                    adoption,
                                    retained.clone(),
                                    commit,
                                ));
                                Ok(())
                            };
                        if let Some(transfer) = transfer.as_mut() {
                            let seed = spare_seed
                                .as_mut()
                                .ok_or("The spare seed plan is unavailable")?;
                            let plan = replacement
                                .take()
                                .ok_or("The spare replacement was already consumed")?;
                            let (successor, proof, ()) = plan.commit(|transition, successor| {
                                transfer.with_validated(transition, successor, &mut || {
                                    seed.with_validated(transition, successor, &mut || {
                                        commit_manager(successor)
                                    })
                                })
                            })?;
                            binding.owner = successor;
                            superseded = Some(proof);
                            Ok(())
                        } else {
                            reservation
                                .with_owner(&original_owner, |_| commit_manager(&original_owner))
                        }
                    },
                );
                if let Err(error) = admitted {
                    if let Some(prepared) = prepared.take() {
                        return state.fail_new_editor(
                            id,
                            prepared.into_plan(),
                            cleanup,
                            rollback,
                            created,
                            error,
                            done,
                            diagram,
                        );
                    }
                    if created {
                        state.finish_creation(id);
                    }
                    done(Err(error));
                    return;
                }
                let Some(tab) = tab else {
                    if created {
                        state.finish_creation(id);
                    }
                    return done(Err("The editor adoption did not commit".into()));
                };
                let retirement_error = superseded.and_then(|proof| {
                    state
                        .supersede_adopted_editor(&reservation, &original_owner, proof)
                        .err()
                });
                let view = target.view;
                let lifetime = state.retain_adopted_editor(
                    view,
                    generation.clone(),
                    binding.reservation.clone(),
                    binding.owner.clone(),
                    binding.closure,
                    rollback,
                );
                if let Err(error) = lifetime {
                    let message = error.message().to_owned();
                    retain_lifetime_failure(
                        &state,
                        Some(error),
                        None,
                        generation.clone(),
                        binding.reservation,
                        binding.owner,
                        None,
                    );
                    drop(cleanup);
                    if created {
                        state.finish_creation(id);
                    }
                    return done(Err(message));
                }
                drop(cleanup);
                if created {
                    state.finish_creation(id);
                }
                if let Some(error) = retirement_error {
                    return done(Err(error));
                }
                if let Some(seed) = spare_seed.as_mut()
                    && let Err(error) = seed.after_commit(&request_cancel)
                {
                    return done(Err(error));
                }
                let active_id = tab.clone();
                let view = target.view;
                state.activate_loaded_authorized(
                    &active_id,
                    authority,
                    target,
                    Vec::new(),
                    request_cancel.clone(),
                    request_cancel,
                    Box::new(move |result| {
                        done(result.map(|()| (tab, view, generation)));
                        drop(diagram);
                    }),
                    None,
                    None,
                );
            }),
        );
    }

    fn finish_owned_open(&self, id: u64, result: Result<NativeOpened, String>) {
        let pending = self.pending_owned.borrow_mut().remove(&id);
        let Some(pending) = pending else {
            self.close_owned_result(result);
            return;
        };
        let PendingOwnedOpen {
            epoch,
            cancel,
            generation,
            step,
            done,
            diagram,
            generated_cleanup,
        } = pending;
        let done = match done {
            Completion::Plain(done) => done,
            Completion::Generated(done) => {
                let aborted = self.owned_abort.borrow_mut().remove(&id);
                return self.finish_generated_open(
                    id,
                    epoch,
                    generation,
                    step,
                    result,
                    generated_cleanup,
                    aborted,
                    done,
                );
            }
            Completion::Saved(done) => {
                self.close_owned_result(result);
                self.cancel_owned_step(step);
                done(Err("The saved open was settled outside its owner".into()));
                drop(diagram);
                return;
            }
            Completion::Replied => {
                if let Some(coordinator) = generated_cleanup {
                    let error = self
                        .owned_abort
                        .borrow_mut()
                        .remove(&id)
                        .unwrap_or_else(|| "The generated open already replied".into());
                    return self.finish_generated_open(
                        id,
                        epoch,
                        generation,
                        step,
                        result,
                        Some(coordinator),
                        Some(error),
                        Box::new(|_| {}),
                    );
                }
                self.close_owned_result(result);
                self.cancel_owned_step(step);
                return;
            }
        };
        let aborted = self.owned_abort.borrow_mut().remove(&id);
        if let Some(error) = aborted {
            match step {
                Step::Hydrate { plan, .. } | Step::NativeHydrate { plan, .. } => {
                    self.inner
                        .borrow_mut()
                        .manager_call(|manager, _| manager.cancel_tab_load(plan));
                    match result {
                        Ok(NativeOpened::Hydrated(loaded)) => {
                            return self.discard_hydration(
                                loaded.attempt,
                                loaded.discard,
                                error,
                                done,
                                diagram,
                            );
                        }
                        Err(cleanup_error) => {
                            done(Err(format!("{error}; {cleanup_error}")));
                            drop(diagram);
                            return;
                        }
                        Ok(other) => {
                            self.close_owned_result(Ok(other));
                            done(Err(error));
                            drop(diagram);
                            return;
                        }
                    }
                }
                other => {
                    self.close_owned_result(result);
                    self.cancel_owned_step(other);
                    done(Err(error));
                    drop(diagram);
                    return;
                }
            }
        }
        let step = match step {
            Step::Hydrate {
                plan,
                origin,
                startup,
                prepared,
            } => {
                return self.finish_hydration(
                    epoch,
                    plan,
                    origin,
                    startup.map(|startup| *startup),
                    None,
                    prepared,
                    cancel,
                    result,
                    done,
                    diagram,
                );
            }
            Step::NativeHydrate {
                plan,
                command,
                shell,
                prepared,
            } => {
                return self.finish_hydration(
                    epoch,
                    plan,
                    shell,
                    None,
                    Some(command),
                    prepared,
                    cancel,
                    result,
                    done,
                    diagram,
                );
            }
            other => other,
        };
        if epoch != self.manager_epoch.get() {
            self.close_owned_result(result);
            self.cancel_owned_step(step);
            done(Err(
                "the shell window changed while opening the document".into()
            ));
            drop(diagram);
            return;
        }
        let result = match step {
            Step::GeneratedLoaded { id, view } => match result {
                Ok(NativeOpened::Generated(ready))
                    if ready.view.view() == view
                        && ready.view.created().same_generation(&generation)
                        && generation.is_active() =>
                {
                    Ok((id, view, generation))
                }
                other => {
                    self.close_owned_result(other);
                    self.hooks.rollback_generated_seed_owned(view, &generation);
                    self.cancel_owned_step(Step::GeneratedLoaded { id, view });
                    Err("The generated editor failed to load or was replaced".into())
                }
            },
            Step::GeneratedFailed { plan, view } => {
                if let Some(plan) = plan {
                    self.inner
                        .borrow_mut()
                        .manager_call(|manager, host| manager.cancel_editor_open(host, plan));
                }
                self.hooks.rollback_generated_seed_owned(view, &generation);
                self.close_owned_result(result);
                Err("The generated initialization failed".into())
            }
            Step::New(plan) => {
                self.cancel_owned_step(Step::New(plan));
                match result {
                    Err(error) => Err(error),
                    Ok(other) => {
                        self.close_owned_result(Ok(other));
                        Err("unexpected editor-open completion".into())
                    }
                }
            }
            Step::Hydrate { plan, .. } | Step::NativeHydrate { plan, .. } => {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                self.close_owned_result(result);
                Err("The recovery completion was already consumed".into())
            }
        };
        done(result);
        drop(diagram);
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_hydration(
        &self,
        epoch: u64,
        plan: PreparedTabLoad,
        origin: NativeRequestOwner,
        startup: Option<super::super::activation::StartupRecoveryConstraint>,
        command: Option<shell_contract::ipc::NativeInvocationOperation>,
        prepared: Option<PreparedSurvivorHydration>,
        cancel: CancelToken,
        result: Result<NativeOpened, String>,
        done: Done,
        diagram: Option<DiagramCheckGuard>,
    ) {
        let id = plan.context().tab_id.clone();
        let loaded = match result {
            Ok(NativeOpened::Hydrated(loaded)) => loaded,
            Ok(other) => {
                self.close_owned_result(Ok(other));
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                done(Err("unexpected restored-tab completion".into()));
                return;
            }
            Err(error) => {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                done(Err(error));
                return;
            }
        };
        let super::super::activation::OwnedLoadedTabView {
            loaded,
            owner: target,
            document,
            discard,
            closure,
            attempt,
        } = loaded;
        let view = loaded.view;
        let generation = match target.context.renderer().cloned() {
            Some(generation) if target.view == view => generation,
            _ => {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                return self.discard_hydration(
                    attempt,
                    discard,
                    "The recovered view has no matching native owner".into(),
                    done,
                    diagram,
                );
            }
        };
        if !closure.reservation.matches(view, &generation) {
            self.inner
                .borrow_mut()
                .manager_call(|manager, _| manager.cancel_tab_load(plan));
            return self.discard_hydration(
                attempt,
                discard,
                "The recovered view has a mismatched closure reservation".into(),
                done,
                diagram,
            );
        }
        let shell = match (&prepared, self.shell_window()) {
            (Some(captured), Some(current)) if captured.shell_window == current => {
                captured.shell_window
            }
            (None, Some(current)) => current,
            _ => {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                return self.discard_hydration(
                    attempt,
                    discard,
                    "The captured shell window is unavailable".into(),
                    done,
                    diagram,
                );
            }
        };
        if epoch != self.manager_epoch.get() {
            self.inner
                .borrow_mut()
                .manager_call(|manager, _| manager.cancel_tab_load(plan));
            return self.discard_hydration(
                attempt,
                discard,
                "The shell window changed while opening the document".into(),
                done,
                diagram,
            );
        }
        let state = self.rc();
        let request_cancel = cancel.clone();
        let request_pending = startup
            .as_ref()
            .map(|request| request.pending.clone())
            .or_else(|| prepared.as_ref().map(|value| value.pending.clone()))
            .unwrap_or_else(|| cancel.clone());
        let authority = match (&startup, command) {
            (Some(request), None) => {
                super::super::activation::TabActivationAuthority::StartupRecovery {
                    startup: request.startup.clone(),
                    shell: request.shell.clone(),
                    child: document.clone(),
                    owner: target.clone(),
                }
            }
            (None, Some(command)) => {
                super::super::activation::TabActivationAuthority::CommandRecovery {
                    command,
                    child: document.clone(),
                    owner: target.clone(),
                }
            }
            (None, None) => super::super::activation::TabActivationAuthority::NativeInvocation {
                document: document.clone(),
                owner: target.clone(),
            },
            (Some(_), Some(_)) => {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                return self.discard_hydration(
                    attempt,
                    discard,
                    "Conflicting recovery authorities".into(),
                    done,
                    diagram,
                );
            }
        };
        let authority_for_gate = authority.clone();
        let mut participants = startup
            .as_ref()
            .map(|request| request.participants.clone())
            .unwrap_or_else(|| vec![origin]);
        if let Some(captured) = &prepared {
            participants.push(captured.shell_owner.clone());
        }
        self.hooks.adopt_recovery_view_owned(
            shell,
            authority.clone(),
            target.clone(),
            participants.clone(),
            cancel.clone(),
            request_pending.clone(),
            Box::new(move |result| {
                if let Err(error) = result {
                    state
                        .inner
                        .borrow_mut()
                        .manager_call(|manager, _| manager.cancel_tab_load(plan));
                    return state.discard_hydration(attempt, discard, error, done, diagram);
                }
                let Some(effects) = state
                    .services
                    .hosts
                    .get::<shell_contract::hosts::OwnedEffectsHost>()
                    .cloned()
                else {
                    state
                        .inner
                        .borrow_mut()
                        .manager_call(|manager, _| manager.cancel_tab_load(plan));
                    return state.discard_hydration(
                        attempt,
                        discard,
                        "Owned recovery adoption is unavailable".into(),
                        done,
                        diagram,
                    );
                };
                let mut plan = Some(plan);
                let mut adoption = None;
                let admitted = super::super::activation::driver::admit(
                    &effects,
                    &authority_for_gate,
                    &participants,
                    &request_cancel,
                    &request_pending,
                    || {
                        closure.reservation.with_owner(&closure.owner, |_| {
                            let mut inner = state.inner.borrow_mut();
                            let manager = inner
                                .manager
                                .as_mut()
                                .ok_or("The shell window is unavailable")?;
                            let plan = plan
                                .take()
                                .ok_or("The recovery adoption was already consumed")?;
                            adoption = Some(manager.resume_tab_load_settled(plan, view)?);
                            Ok(())
                        })
                    },
                );
                if let Err(error) = admitted {
                    if let Some(plan) = plan.take() {
                        state
                            .inner
                            .borrow_mut()
                            .manager_call(|manager, _| manager.cancel_tab_load(plan));
                    }
                    return state.discard_hydration(attempt, discard, error, done, diagram);
                }
                if adoption != Some(view) {
                    return state.discard_hydration(
                        attempt,
                        discard,
                        "The recovery adoption did not commit".into(),
                        done,
                        diagram,
                    );
                }
                if let Some(prepared) = prepared {
                    let lifetime = state.retain_adopted_editor(
                        view,
                        generation.clone(),
                        closure.reservation.clone(),
                        closure.owner.clone(),
                        closure.closure.clone(),
                        attempt.rollback_store(),
                    );
                    let (handle, lifetime) = match lifetime {
                        Ok(handle) => (Some(handle), None),
                        Err(failure) => (None, Some(failure)),
                    };
                    let adopted = match handle.as_ref() {
                        Some(handle) if handle.is_unclosed() == Ok(true) => closure
                            .reservation
                            .with_owner(&closure.owner, |_| attempt.adopted()),
                        Some(_) => Err("The adopted survivor is retiring".into()),
                        None => Ok(()),
                    };
                    let error = lifetime
                        .as_ref()
                        .map(|failure| failure.message().to_owned())
                        .or_else(|| adopted.as_ref().err().cloned());
                    let outcome = match error {
                        Some(error) => {
                            SurvivorHydration::AdoptedPending(Box::new(AdoptedSurvivorPending {
                                id: id.clone(),
                                owner: target,
                                generation: generation.clone(),
                                closure,
                                attempt,
                                error,
                                lifetime_secured: handle.is_some(),
                                attempt_secured: handle.is_some() && adopted.is_ok(),
                                handle,
                                lifetime,
                            }))
                        }
                        None => SurvivorHydration::Ready {
                            id: id.clone(),
                            owner: target,
                            generation: generation.clone(),
                        },
                    };
                    *prepared.outcome.borrow_mut() = Some(outcome);
                    done(Ok((id, view, generation)));
                    drop(diagram);
                    return;
                }
                let lifetime = state.retain_adopted_editor(
                    view,
                    generation.clone(),
                    closure.reservation.clone(),
                    closure.owner.clone(),
                    closure.closure,
                    attempt.rollback_store(),
                );
                let handle = match lifetime {
                    Ok(handle) => handle,
                    Err(error) => {
                        let message = error.message().to_owned();
                        retain_lifetime_failure(
                            &state,
                            Some(error),
                            None,
                            generation.clone(),
                            closure.reservation,
                            closure.owner,
                            Some((attempt, discard)),
                        );
                        return done(Err(message));
                    }
                };
                let adopted = if handle.is_unclosed() == Ok(true) && generation.is_active() {
                    closure
                        .reservation
                        .with_owner(&closure.owner, |_| attempt.adopted())
                } else {
                    Err("The adopted recovery editor is retiring".into())
                };
                if let Err(error) = adopted {
                    retain_lifetime_failure(
                        &state,
                        None,
                        Some(handle),
                        generation.clone(),
                        closure.reservation,
                        closure.owner,
                        Some((attempt, discard)),
                    );
                    return done(Err(error));
                }
                drop(discard);
                let activation_id = id.clone();
                let completed = Box::new(move |result: Result<(), String>| {
                    done(result.map(|()| (id, view, generation)));
                    drop(diagram);
                });
                state.activate_loaded_authorized(
                    &activation_id,
                    authority,
                    target,
                    participants,
                    request_cancel,
                    request_pending,
                    completed,
                    None,
                    None,
                );
            }),
        );
    }

    pub fn report_recovery_cleanup_failure(&self, error: String) {
        self.hooks.warn("[recovery] cleanup is pending:", &error);
        let waiters = std::mem::take(&mut *self.creation_cleanup_waiters.borrow_mut());
        for waiter in waiters {
            waiter(Err(error.clone()));
        }
    }
    pub(crate) fn retry_recovery_cleanups(&self) {
        let attempts = self.retained_recovery_cleanups.borrow().clone();
        for attempt in attempts {
            attempt.retry_cleanup();
        }
    }

    fn discard_hydration(
        &self,
        attempt: std::rc::Rc<dyn super::super::activation::RecoveryAttempt>,
        discard: super::super::activation::RecoveryCleanup,
        error: String,
        done: Done,
        diagram: Option<DiagramCheckGuard>,
    ) {
        self.retained_recovery_cleanups
            .borrow_mut()
            .push(attempt.clone());
        let state = self.rc();
        let completed = state.clone();
        let completed_attempt = attempt.clone();
        let reply = std::rc::Rc::new(RefCell::new(Some(done)));
        let failed = reply.clone();
        let original = error.clone();
        attempt.fail(
            error,
            Some(discard),
            std::rc::Rc::new(move |error| {
                state.report_recovery_cleanup_failure(error.clone());
                let done = failed.borrow_mut().take();
                if let Some(done) = done {
                    done(Err(format!("{original}; cleanup failed: {error}")));
                }
            }),
            Box::new(move |result| {
                completed
                    .retained_recovery_cleanups
                    .borrow_mut()
                    .retain(|entry| !std::rc::Rc::ptr_eq(entry, &completed_attempt));
                let error = match result {
                    Err(error) => error,
                    Ok(()) => "Recovery cleanup completed without its failure result".into(),
                };
                let done = reply.borrow_mut().take();
                if let Some(done) = done {
                    done(Err(error));
                }
                completed.finish_creation_drain_if_ready();
                drop(diagram);
            }),
        );
    }

    fn close_owned_result(&self, result: Result<NativeOpened, String>) {
        match result {
            Ok(NativeOpened::Hydrated(loaded)) => self.discard_hydration(
                loaded.attempt,
                loaded.discard,
                "The recovery completion no longer has an owner".into(),
                Box::new(|result| {
                    if let Err(error) = result {
                        eprintln!("[recovery] orphaned opener: {error}");
                    }
                }),
                None,
            ),
            Ok(NativeOpened::Generated(ready)) => {
                let view = ready.view.view();
                self.retain_creation_cleanup(
                    ready.cleanup,
                    Box::new(|error| {
                        eprintln!("[shell] orphaned generated cleanup failed: {error}")
                    }),
                    Box::new(move || {
                        eprintln!("[shell] discarded orphaned generated view {}", view.0)
                    }),
                );
            }
            _ => {}
        }
    }

    fn cancel_owned_step(&self, step: Step) {
        match step {
            Step::New(plan) => {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, host| manager.cancel_editor_open(host, plan));
            }
            Step::GeneratedFailed { plan, .. } => drop(plan),
            Step::GeneratedLoaded { id, view } => {
                self.inner.borrow_mut().manager_call(|manager, host| {
                    if manager.web_contents_for_tab(&id) == Some(view) {
                        manager.close_tab_without_prompt(host, &id);
                    }
                });
            }
            Step::Hydrate { plan, .. } | Step::NativeHydrate { plan, .. } => {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
            }
        }
    }

    pub(crate) fn abort_owned_opens(&self, message: &str) {
        let pending = std::mem::take(&mut *self.pending_owned.borrow_mut());
        for (id, pending) in pending {
            if matches!(
                &pending.step,
                Step::New(_)
                    | Step::GeneratedLoaded { .. }
                    | Step::GeneratedFailed { .. }
                    | Step::Hydrate { .. }
                    | Step::NativeHydrate { .. }
            ) {
                self.owned_abort.borrow_mut().insert(id, message.to_owned());
                self.pending_owned.borrow_mut().insert(id, pending);
                continue;
            }
            self.cancel_owned_step(pending.step);
            pending.done.fail(message.to_owned());
            drop(pending.diagram);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reveal_owned(
        &self,
        id: String,
        view: ViewId,
        detached: bool,
        _home: bool,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        epoch: u64,
        diagram: Option<DiagramCheckGuard>,
        done: Done,
    ) {
        if epoch != self.manager_epoch.get() || cancel.is_cancelled() {
            done(Err("the owned tab operation is no longer active".into()));
            return;
        }
        let Some(generation) = self.services.views.renderer_generation(view) else {
            return done(Err("the target editor generation is unavailable".into()));
        };
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
        else {
            return done(Err("Owned activation is unavailable".into()));
        };
        let target = match effects.capture_owner(view, &generation) {
            Ok(target) => target,
            Err(error) => return done(Err(error)),
        };
        let selected = id.clone();
        let complete = Box::new(move |result: Result<(), String>| {
            done(result.map(|()| (id, view, generation)));
            drop(diagram);
        });
        if detached {
            self.reveal_loaded_owned_many(
                &selected,
                owner,
                target,
                Vec::new(),
                cancel.clone(),
                cancel,
                complete,
            );
        } else {
            self.activate_loaded_owned_many(
                &selected,
                owner,
                target,
                Vec::new(),
                cancel.clone(),
                cancel,
                complete,
            );
        }
    }

    pub fn reveal_tab_owned(
        &self,
        id: &str,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.activate_owned(
            id.to_owned(),
            owner,
            cancel,
            self.manager_epoch.get(),
            None,
            Box::new(move |result| done(result.map(|_| ()))),
        );
    }

    fn activate_owned(
        &self,
        id: String,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        epoch: u64,
        diagram: Option<DiagramCheckGuard>,
        done: Done,
    ) {
        let prepared = self.inner.borrow_mut().manager_call(|manager, _| {
            let view = manager.web_contents_for_tab(&id);
            manager.prepare_tab_load(&id).map(|plan| (view, plan))
        });
        let (view, plan) = match prepared {
            Some(Ok(prepared)) => prepared,
            Some(Err(error)) => {
                done(Err(error));
                return;
            }
            None => {
                done(Err("the shell window is not available".into()));
                return;
            }
        };
        if let Some(plan) = plan {
            let Some(generation) = owner.context.renderer().cloned() else {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                return done(Err("The request has no captured renderer".into()));
            };
            let operation = match self.owned_id() {
                Ok(id) => id,
                Err(error) => {
                    self.inner
                        .borrow_mut()
                        .manager_call(|manager, _| manager.cancel_tab_load(plan));
                    done(Err(error));
                    return;
                }
            };
            if let Err(error) = self.begin_creation(operation) {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                return done(Err(error));
            }
            let completed = self.rc();
            let done: Done = Box::new(move |result| {
                completed.finish_creation(operation);
                done(result);
            });
            let opener = *plan.opener();
            let context = plan.context().clone();
            self.pending_owned.borrow_mut().insert(
                operation,
                PendingOwnedOpen {
                    epoch,
                    cancel: cancel.clone(),
                    generation,
                    step: Step::Hydrate {
                        plan,
                        origin: owner.clone(),
                        startup: None,
                        prepared: None,
                    },
                    done: Completion::Plain(done),
                    generated_cleanup: None,
                    diagram,
                },
            );
            let state = self.this.clone();
            self.hooks.open_opener_owned(
                &opener,
                &context,
                owner,
                cancel.clone(),
                cancel,
                Box::new(move |result| {
                    if let Some(state) = state.upgrade() {
                        let pending = state.pending_owned.borrow().contains_key(&operation);
                        if pending {
                            return state
                                .finish_owned_open(operation, result.map(NativeOpened::Hydrated));
                        }
                    }
                    if let Ok(loaded) = result {
                        loaded.attempt.fail(
                            "The requesting tab was closed".into(),
                            Some(loaded.discard),
                            std::rc::Rc::new(|error| {
                                eprintln!("[recovery] cancelled cleanup failed: {error}")
                            }),
                            Box::new(move |result| {
                                if let Err(error) = result {
                                    eprintln!("[recovery] cancelled opener: {error}");
                                }
                            }),
                        );
                    }
                }),
            );
        } else if let Some(view) = view {
            self.reveal_owned(id, view, false, false, owner, cancel, epoch, diagram, done);
        } else {
            done(Err("the target tab has no editor view".into()));
        }
    }

    /// Hydrate the exact pre-close survivor without activating it. The returned
    /// owner is the one supplied by the adopted native editor, never a view lookup.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn hydrate_generated_survivor(
        &self,
        opener: main_core::shell::tab_manager::Opener,
        context: main_core::shell::tab_manager::UnloadedTabOpenContext,
        source: NativeRequestOwner,
        shell: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<SurvivorHydration, String>)>,
    ) {
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err("Owned recovery hydration is unavailable".into()));
        };
        let Some(generation) = source.context.renderer().cloned() else {
            return done(Err("The request has no captured renderer".into()));
        };
        let mut plan = None;
        let mut captured_window = None;
        let admitted = effects.with_owned_effect(&source, &shell, &cancel, &pending, &mut || {
            let mut inner = self.inner.borrow_mut();
            let window = inner
                .bridge
                .shell
                .ok_or("The shell window is unavailable")?;
            if window.view != shell.view {
                return Err("The captured shell window changed".into());
            }
            captured_window = Some(window);
            let manager = inner
                .manager
                .as_mut()
                .ok_or("The shell window is unavailable")?;
            plan = Some(manager.prepare_captured_tab_load(&opener, &context)?);
            Ok(())
        });
        if let Err(error) = admitted {
            if let Some(plan) = plan.take() {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
            }
            return done(Err(error));
        }
        let Some(plan) = plan else {
            return done(Err("The survivor hydration was not admitted".into()));
        };
        let Some(shell_window) = captured_window else {
            self.inner
                .borrow_mut()
                .manager_call(|manager, _| manager.cancel_tab_load(plan));
            return done(Err("The shell window was not admitted".into()));
        };
        let operation = match self.owned_id() {
            Ok(id) => id,
            Err(error) => {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                return done(Err(error));
            }
        };
        if let Err(error) = self.begin_creation(operation) {
            self.inner
                .borrow_mut()
                .manager_call(|manager, _| manager.cancel_tab_load(plan));
            return done(Err(error));
        }
        let slot = std::rc::Rc::new(RefCell::new(None));
        let answer = slot.clone();
        let completed = self.rc();
        let finish: Done = Box::new(move |result| {
            completed.finish_creation(operation);
            let outcome = answer.borrow_mut().take();
            let result = match outcome {
                Some(outcome) => Ok(outcome),
                None => {
                    result.and_then(|_| Err("The adopted survivor owner is unavailable".into()))
                }
            };
            done(result);
        });
        let epoch = self.manager_epoch.get();
        self.pending_owned.borrow_mut().insert(
            operation,
            PendingOwnedOpen {
                epoch,
                cancel: cancel.clone(),
                generation,
                step: Step::Hydrate {
                    plan,
                    origin: source.clone(),
                    startup: None,
                    prepared: Some(PreparedSurvivorHydration {
                        outcome: slot,
                        pending: pending.clone(),
                        shell_owner: shell,
                        shell_window,
                    }),
                },
                done: Completion::Plain(finish),
                generated_cleanup: None,
                diagram: None,
            },
        );
        let state = self.rc();
        self.hooks.open_opener_owned(
            &opener,
            &context,
            source,
            cancel,
            pending,
            Box::new(move |result| {
                if state.pending_owned.borrow().contains_key(&operation) {
                    return state.finish_owned_open(operation, result.map(NativeOpened::Hydrated));
                }
                state.close_owned_result(result.map(NativeOpened::Hydrated));
            }),
        );
    }

    pub fn open_document_chat_owned(
        &self,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        path: Option<String>,
        done: Box<dyn FnOnce(Result<Option<ViewId>, String>) + Send>,
    ) {
        let epoch = self.manager_epoch.get();
        let state = self.this.clone();
        let source = owner.clone();
        let request_cancel = cancel.clone();
        let finish: Routed = Box::new(move |result: Result<Option<ViewId>, String>| match result {
            Ok(None) => {
                if let Some(state) = state.upgrade() {
                    state.home_owned(owner, cancel, epoch, done);
                } else {
                    done(Err("the tab manager is unavailable".into()));
                }
            }
            result => done(result),
        });
        let Some(path) = path else {
            finish(Ok(None));
            return;
        };
        let state = self.this.clone();
        self.hooks.prepare_reopen_owned(
            path,
            source.clone(),
            source,
            request_cancel.clone(),
            request_cancel,
            Box::new(move |result| {
                let Some(state) = state.upgrade() else {
                    finish(Err("the tab manager is unavailable".into()));
                    return;
                };
                match result {
                    Err(error) => finish(Err(error)),
                    Ok((path, fallback, owner, cancel)) => {
                        let retry_state = state.this.clone();
                        let retry_owner = owner.clone();
                        let retry_cancel = cancel.clone();
                        state.route_owned(
                            path,
                            owner,
                            cancel,
                            epoch,
                            None,
                            Box::new(move |result| {
                                if matches!(result, Ok(None))
                                    && let Some(path) = fallback
                                {
                                    if let Some(state) = retry_state.upgrade() {
                                        state.route_owned(
                                            path,
                                            retry_owner,
                                            retry_cancel,
                                            epoch,
                                            None,
                                            finish,
                                        );
                                    } else {
                                        finish(Err("the tab manager is unavailable".into()));
                                    }
                                    return;
                                }
                                finish(result);
                            }),
                        );
                    }
                }
            }),
        );
    }

    fn home_owned(&self, owner: NativeRequestOwner, cancel: CancelToken, epoch: u64, done: Routed) {
        let shell = self.inner.borrow().bridge.shell;
        let Some(shell) = shell else {
            done(Err("the shell window is not available".into()));
            return;
        };
        self.reveal_owned(
            "home".into(),
            shell.view,
            false,
            true,
            owner,
            cancel,
            epoch,
            None,
            Box::new(move |result| done(result.map(|(_, view, _)| Some(view)))),
        );
    }

    pub fn route_document_path_owned(
        &self,
        path: String,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        done: Box<dyn FnOnce(Result<Option<ViewId>, String>)>,
    ) {
        self.route_owned(path, owner, cancel, self.manager_epoch.get(), None, done);
    }

    /// `routeDocumentPath` for a caller holding an activation authority (the startup launch):
    /// the authority, its cancel and pending tokens are admitted before the route starts, then
    /// the route runs for the authority's captured owner. `done` runs once, after the document
    /// is shown, found already open, or the route ended without one.
    pub fn route_document_path_authorized(
        &self,
        path: String,
        authority: super::super::activation::TabActivationAuthority,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<Option<ViewId>, String>)>,
    ) {
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err("Owned route effects are unavailable".into()));
        };
        let admitted = super::super::activation::driver::admit(
            &effects,
            &authority,
            &[],
            &cancel,
            &pending,
            || Ok(()),
        );
        if let Err(error) = admitted {
            return done(Err(error));
        }
        let owner = authority.owner().clone();
        self.route_owned(path, owner, cancel, self.manager_epoch.get(), None, done);
    }

    fn route_owned(
        &self,
        path: String,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        epoch: u64,
        diagram: Option<DiagramCheckGuard>,
        done: Routed,
    ) {
        if epoch != self.manager_epoch.get() || cancel.is_cancelled() {
            done(Err("the owned tab operation is no longer active".into()));
            return;
        }
        if !std::path::Path::new(&path).exists() {
            done(Ok(None));
            return;
        }
        let detached = {
            let mut state = self.inner.borrow_mut();
            let super::super::state::Inner {
                detached, bridge, ..
            } = &mut *state;
            detached.find_detached_tab_by_path(bridge, &path)
        };
        if let Some(detached) = detached {
            self.reveal_owned(
                detached.id,
                detached.view,
                true,
                false,
                owner,
                cancel,
                epoch,
                diagram,
                Box::new(move |result| done(result.map(|(_, view, _)| Some(view)))),
            );
            return;
        }
        if self.inner.borrow().manager.is_none() {
            done(Ok(None));
            return;
        }
        let Some(kind) = document_kind(&path) else {
            self.hooks.notify_unsupported_file(&path);
            done(Ok(None));
            return;
        };
        let existing = self
            .inner
            .borrow_mut()
            .manager_call(|manager, host| {
                if kind == TabKind::Diagram {
                    manager.find_diagram_tab_by_path(host, Some(&path))
                } else {
                    manager.find_tab_of_kind_by_path(kind, Some(&path))
                }
            })
            .flatten();
        if let Some(id) = existing {
            self.hooks.record_recent_file(&path);
            self.activate_owned(
                id,
                owner,
                cancel,
                epoch,
                diagram,
                Box::new(move |result| done(result.map(|(_, view, _)| Some(view)))),
            );
            return;
        }
        if kind == TabKind::Diagram && diagram.is_none() {
            let key = jscompat::node_path::host::resolve(&[&path]);
            let key = if cfg!(target_os = "linux") {
                key
            } else {
                key.to_lowercase()
            };
            if !DIAGRAM_CHECKING.with(|checking| checking.borrow_mut().insert(key.clone())) {
                let state = self.this.clone();
                DIAGRAM_WAITERS.with(|waiting| {
                    waiting
                        .borrow_mut()
                        .entry(key)
                        .or_default()
                        .push(Box::new(move || {
                            if let Some(state) = state.upgrade() {
                                state.route_owned(path, owner, cancel, epoch, None, done);
                            } else {
                                done(Err("the tab manager is unavailable".into()));
                            }
                        }))
                });
                return;
            }
            let guard = DiagramCheckGuard::new(key);
            let state = self.this.clone();
            self.hooks.diagram_open_refusal_owned(
                path.clone(),
                owner.clone(),
                Vec::new(),
                cancel.clone(),
                cancel.clone(),
                Box::new(move |result| {
                    let Some(state) = state.upgrade() else {
                        done(Err("the tab manager is unavailable".into()));
                        return;
                    };
                    if epoch != state.manager_epoch.get() || cancel.is_cancelled() {
                        done(Err("the owned tab operation is no longer active".into()));
                        return;
                    }
                    match result {
                        Err(error) => done(Err(error)),
                        Ok(Some(reason)) => {
                            let detail = Some(jscompat::node_path::host::basename(&path, None));
                            state.hooks.show_app_warning_owned(
                                reason,
                                detail,
                                owner,
                                Vec::new(),
                                cancel.clone(),
                                cancel,
                                Box::new(move |result| {
                                    drop(guard);
                                    done(result.map(|()| None));
                                }),
                            );
                        }
                        Ok(None) => {
                            state.route_owned(path, owner, cancel, epoch, Some(guard), done)
                        }
                    }
                }),
            );
            return;
        }
        let Some(request) = editor_request_for_path(kind, &path) else {
            done(Ok(None));
            return;
        };
        self.hooks.record_recent_file(&path);
        let state = self.this.clone();
        self.open_editor_owned_at(
            request,
            owner,
            cancel,
            epoch,
            diagram,
            Box::new(move |result| {
                if kind == TabKind::Sheets
                    && result.is_ok()
                    && let Some(state) = state.upgrade()
                {
                    state.hooks.start_queued_workbook_nudge();
                }
                done(result.map(|(_, view, _)| Some(view)));
            }),
        );
    }
}
