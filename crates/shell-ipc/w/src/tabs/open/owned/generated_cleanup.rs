use std::sync::{Arc, Mutex, Weak};

use shell_contract::ipc::{
    NativeBrowserCloseClaim, NativeBrowserClosePermit, NativeBrowserRetirement,
};

use crate::tabs::host::{NativeBrowserBinding, ViewCreationCleanup};
use main_core::shell::tab_manager::{
    GeneratedRemovalPreview, PreparedTabActivation, TabRemovalState,
};
use shell_contract::ViewId;

use super::GeneratedCleanupAuthority;
use crate::tabs::activation::driver::{
    self, ActivationParticipants, PreparedCommittedActivationBatch,
};
use crate::tabs::activation::{ReservedSplitMemoryRevision, TabActivationAuthority};
use crate::tabs::host::RetainedSpareBinding;
use crate::tabs::state::TabsState;

type PrecloseDone = Box<dyn FnOnce(Result<(), String>) + Send>;
struct PrecloseMailbox {
    barrier: std::rc::Rc<dyn Fn(PrecloseDone)>,
    failure: std::rc::Rc<dyn Fn(String)>,
}
thread_local! {
    static PRECLOSE_MAILBOXES: std::cell::RefCell<std::collections::HashMap<u64, PrecloseMailbox>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}
static NEXT_PRECLOSE_MAILBOX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(super) struct PrecloseRegistration(u64);
impl Drop for PrecloseRegistration {
    fn drop(&mut self) {
        PRECLOSE_MAILBOXES.with(|entries| {
            entries.borrow_mut().remove(&self.0);
        });
    }
}

pub(super) fn preclose_mailbox(
    core: Arc<dyn shell_contract::runtime::Thread>,
    barrier: std::rc::Rc<dyn Fn(PrecloseDone)>,
    failure: std::rc::Rc<dyn Fn(String)>,
) -> Result<
    (
        PrecloseRegistration,
        crate::tabs::host::GeneratedPrecloseBarrier,
        crate::tabs::host::GeneratedPrecloseFailure,
    ),
    String,
> {
    let id = NEXT_PRECLOSE_MAILBOX
        .try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |id| id.checked_add(1),
        )
        .map_err(|_| "The generated preclose identity is unavailable")?;
    PRECLOSE_MAILBOXES.with(|entries| {
        entries
            .borrow_mut()
            .insert(id, PrecloseMailbox { barrier, failure });
    });
    let barrier_core = core.clone();
    let post_barrier = Arc::new(move |done: PrecloseDone| {
        barrier_core.post(Box::new(move || {
            let barrier = PRECLOSE_MAILBOXES
                .with(|entries| entries.borrow().get(&id).map(|entry| entry.barrier.clone()));
            match barrier {
                Some(barrier) => barrier(done),
                None => done(Err("The generated preclose operation is unavailable".into())),
            }
        }));
    });
    let post_failure = Arc::new(move |error: String| {
        core.post(Box::new(move || {
            let failure = PRECLOSE_MAILBOXES
                .with(|entries| entries.borrow().get(&id).map(|entry| entry.failure.clone()));
            if let Some(failure) = failure {
                failure(error);
            }
        }));
    });
    Ok((PrecloseRegistration(id), post_barrier, post_failure))
}

pub(super) struct PreparedRemoval {
    pub activation: Option<PreparedTabActivation<ViewId, RetainedSpareBinding>>,
    pub removal: TabRemovalState,
    pub batch: PreparedCommittedActivationBatch,
    pub reserved_revision: Option<ReservedSplitMemoryRevision>,
}
pub(super) fn survivor_to_hydrate(
    authority: &GeneratedCleanupAuthority,
) -> Result<
    Option<(
        main_core::shell::tab_manager::Opener,
        main_core::shell::tab_manager::UnloadedTabOpenContext,
    )>,
    String,
> {
    use main_core::shell::tab_manager::GeneratedRemovalSurvivor;
    let preview = &authority.preview;
    let Some(target) = preview.removal().activate.as_deref() else {
        return Ok(None);
    };
    for survivor in preview.survivors() {
        match survivor {
            GeneratedRemovalSurvivor::Unloaded { opener, context } if context.tab_id == target => {
                return Ok(Some((opener, context)));
            }
            GeneratedRemovalSurvivor::Loaded { id, .. }
            | GeneratedRemovalSurvivor::Viewless { id }
                if id == target =>
            {
                return Ok(None);
            }
            _ => {}
        }
    }
    Err("The captured survivor target is unavailable".into())
}

pub(super) fn validate_survivors(
    authority: &GeneratedCleanupAuthority,
    current: &GeneratedRemovalPreview<ViewId, RetainedSpareBinding>,
    hydrated: Option<(&str, &shell_contract::ipc::NativeRequestOwner)>,
) -> Result<(), String> {
    use main_core::shell::tab_manager::GeneratedRemovalSurvivor;
    let original = authority.preview.survivors();
    let current = current.survivors();
    if original.len() != current.len() {
        return Err("The captured survivor set changed".into());
    }
    for (old, new) in original.iter().zip(&current) {
        let unchanged = old == new;
        let newly_loaded = match (old, new, hydrated) {
            (
                GeneratedRemovalSurvivor::Unloaded { context, .. },
                GeneratedRemovalSurvivor::Loaded { id, view },
                Some((hydrated_id, owner)),
            ) => context.tab_id == *id && id == hydrated_id && *view == owner.view,
            _ => false,
        };
        if !unchanged && !newly_loaded {
            return Err("A captured survivor changed before generated cleanup".into());
        }
    }
    Ok(())
}

pub(super) fn prepare_removal(
    state: &TabsState,
    authority: &GeneratedCleanupAuthority,
    preview: &GeneratedRemovalPreview<ViewId, RetainedSpareBinding>,
) -> Result<PreparedRemoval, String> {
    if state.manager_epoch.get() != authority.manager_epoch
        || state.shell_window() != Some(authority.shell_window)
    {
        return Err("The captured shell changed before generated cleanup".into());
    }
    let manager = preview.manager();
    let target_id = match &preview.removal().activate {
        Some(id) => id.clone(),
        None => {
            manager
                .list()
                .into_iter()
                .find(|tab| tab.active)
                .ok_or("The surviving active tab is unavailable")?
                .id
        }
    };
    let expected_view = manager.web_contents_for_tab(&target_id);
    let target = match expected_view {
        None => authority.shell.clone(),
        Some(view) if authority.source.view == view => authority.source.clone(),
        Some(view) if authority.shell.view == view => authority.shell.clone(),
        Some(view) => authority
            .participants
            .iter()
            .find(|owner| owner.view == view)
            .cloned()
            .ok_or("The surviving target owner was not captured")?,
    };
    let environment = {
        let mut inner = state.inner.borrow_mut();
        manager.activation_environment(&mut inner.bridge, &target_id)
    };
    let activation = manager.prepare_generated_removal_effects(preview.removal(), environment)?;
    let final_preview = manager.preview_loaded_activation(&activation)?;
    let effects = state
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
        .ok_or("Owned generated cleanup is unavailable")?;
    // Restoration runs after the original request was cancelled, so its effects
    // are admitted for the live shell (which owns its tab strip) under the
    // cleanup lifetime, never under the cancelled request's tokens.
    let effect_authority = TabActivationAuthority::Interactive(authority.shell.clone());
    let mut participants = authority.participants.clone();
    participants.push(target);
    let presentation = driver::prepare_activation_presentation(
        state,
        effects,
        &final_preview,
        activation.effects(),
        authority.shell_window,
        &authority.shell,
        expected_view,
        &effect_authority,
        ActivationParticipants::CapturedOnly(participants),
        &authority.cleanup,
        &authority.cleanup,
    )?;
    let batch = PreparedCommittedActivationBatch {
        authority: effect_authority,
        participants: presentation.participants,
        base_count: presentation.base_count,
        cancel: authority.cleanup.clone(),
        pending: authority.cleanup.clone(),
        effects_host: presentation.effects_host,
        effects: activation.effects().to_vec(),
        native: presentation.native,
        publication: presentation.publication,
        revision: presentation.revision,
    };
    Ok(PreparedRemoval {
        activation: Some(activation),
        removal: preview.removal().clone(),
        batch,
        reserved_revision: presentation.reserved_revision,
    })
}

type Done = Box<dyn FnOnce(Result<NativeBrowserRetirement, String>) + Send>;

struct CloseState {
    permit: Option<NativeBrowserClosePermit>,
    retirement: Option<NativeBrowserRetirement>,
    watching: bool,
    running: bool,
    dispatched: bool,
    dispatching: bool,
    watch_error: Option<String>,
    attempt: u64,
    waiters: Vec<Done>,
}

pub(super) struct BrowserClose {
    binding: NativeBrowserBinding,
    state: Mutex<CloseState>,
}

struct WatchGuard {
    operation: Weak<BrowserClose>,
    pending: bool,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        if !self.pending {
            return;
        }
        let Some(operation) = self.operation.upgrade() else {
            return;
        };
        let error = "The native closure watcher discarded its completion".to_owned();
        let report = {
            let mut state = operation
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.watching = false;
            if state.retirement.is_some() {
                false
            } else if state.dispatching {
                state.watch_error = Some(error.clone());
                false
            } else {
                state.running
            }
        };
        if report {
            operation.finish(Err(error));
        }
    }
}

impl BrowserClose {
    pub(super) fn install_claim(
        &self,
        claim: NativeBrowserCloseClaim<()>,
    ) -> Result<(), (String, NativeBrowserCloseClaim<()>)> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let valid = match &claim {
            NativeBrowserCloseClaim::Won { permit, .. } => permit
                .reservation()
                .same_reservation(&self.binding.reservation),
            NativeBrowserCloseClaim::Superseded(proof) => {
                proof.matches(&self.binding.reservation, &self.binding.owner)
            }
        };
        if !valid
            || state.running
            || state.permit.is_some()
            || state.retirement.is_some()
            || state.dispatched
        {
            return Err((
                "The generated close claim cannot be installed".into(),
                claim,
            ));
        }
        match claim {
            NativeBrowserCloseClaim::Won { permit, .. } => state.permit = Some(permit),
            NativeBrowserCloseClaim::Superseded(proof) => {
                state.retirement = Some(NativeBrowserRetirement::Superseded(proof))
            }
        }
        Ok(())
    }

    fn finish(&self, result: Result<NativeBrowserRetirement, String>) {
        let waiters = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.running = false;
            if let Ok(retirement) = &result {
                state.retirement = Some(retirement.clone());
            }
            std::mem::take(&mut state.waiters)
        };
        for done in waiters {
            done(result.clone());
        }
    }

    fn arm_watch(self: &Arc<Self>) -> Result<(), String> {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.watching || state.retirement.is_some() {
                return Ok(());
            }
            state.watching = true;
        }
        let mut guard = WatchGuard {
            operation: Arc::downgrade(self),
            pending: true,
        };
        let operation = Arc::downgrade(self);
        self.binding.producer.watch(Box::new(move |receipt| {
            let guard = &mut guard;
            guard.pending = false;
            let Some(operation) = operation.upgrade() else {
                return;
            };
            if !receipt.matches(
                &operation.binding.reservation,
                operation.binding.reservation.view(),
                operation.binding.reservation.created(),
            ) {
                operation
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .watching = false;
                operation.finish(Err(
                    "The native closure receipt does not match the generated browser".into(),
                ));
            } else if receipt
                .terminal_owner()
                .is_some_and(|owner| owner.same_owner(&operation.binding.owner))
            {
                operation.finish(Ok(NativeBrowserRetirement::Closed(receipt)));
            } else {
                operation
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .watching = false;
                operation.finish(Err(
                    "The terminal native browser belongs to another owner".into()
                ));
            }
        }))
    }

    fn has_retirement(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retirement
            .is_some()
    }

    fn start(self: &Arc<Self>, done: Done) {
        let attempt = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(retirement) = &state.retirement {
                let retirement = retirement.clone();
                drop(state);
                return done(Ok(retirement));
            }
            if state.running {
                state.waiters.push(done);
                return;
            }
            let Some(attempt) = state.attempt.checked_add(1) else {
                drop(state);
                return done(Err("The native close attempt identity is exhausted".into()));
            };
            state.waiters.push(done);
            state.running = true;
            state.attempt = attempt;
            attempt
        };
        if let Err(error) = self.arm_watch() {
            let report = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.running && state.attempt == attempt
            };
            if report {
                self.finish(Err(error));
            }
            return;
        }
        let permit = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.retirement.is_some()
                || !state.running
                || state.attempt != attempt
                || state.dispatched
            {
                return;
            }
            state.permit.take()
        };
        let Some(permit) = permit else {
            return self.finish(Err(
                "The generated browser close claim is unavailable".into()
            ));
        };
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.running || state.attempt != attempt {
                state.permit = Some(permit);
                return;
            }
            state.dispatching = true;
        }
        match self.binding.producer.dispatch_close(permit) {
            Ok(permit) => {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.permit = Some(permit);
                state.dispatched = true;
                state.dispatching = false;
                let error = state.watch_error.take();
                drop(state);
                if let Some(error) = error {
                    self.finish(Err(error));
                }
            }
            Err((error, permit)) => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                state.permit = Some(permit);
                state.dispatching = false;
                state.watch_error = None;
                drop(state);
                self.finish(Err(error));
            }
        }
    }
}

pub(super) fn claim_browser_close(
    binding: &NativeBrowserBinding,
    actor: Option<&std::rc::Rc<dyn crate::tabs::host::SpareActor>>,
) -> Result<NativeBrowserCloseClaim<()>, String> {
    binding
        .reservation
        .claim_close(&binding.owner, |transition| match actor {
            Some(actor) => actor
                .prepare_close()?
                .with_validated(transition, &mut || Ok(())),
            None => Ok(()),
        })
}

pub(super) fn browser_cleanup(
    binding: NativeBrowserBinding,
) -> (Arc<BrowserClose>, ViewCreationCleanup) {
    let operation = Arc::new(BrowserClose {
        binding,
        state: Mutex::new(CloseState {
            permit: None,
            retirement: None,
            watching: false,
            running: false,
            dispatched: false,
            dispatching: false,
            watch_error: None,
            attempt: 0,
            waiters: Vec::new(),
        }),
    });
    let cleanup = operation.clone();
    (operation, Arc::new(move |done| cleanup.start(done)))
}

enum RemovalPhase {
    Ready,
    Running,
    Retryable,
    Settled(Result<(), String>),
}

enum HydratedSurvivor {
    Unstarted,
    Opening,
    Ready {
        id: String,
        owner: shell_contract::ipc::NativeRequestOwner,
        generation: shell_contract::runtime::RendererGeneration,
    },
    Pending(Box<super::AdoptedSurvivorPending>),
    Failed(String),
}

impl HydratedSurvivor {
    fn ready_owner(
        &self,
    ) -> Result<Option<(&str, &shell_contract::ipc::NativeRequestOwner)>, String> {
        match self {
            Self::Unstarted => Ok(None),
            Self::Ready {
                id,
                owner,
                generation,
            } if generation.is_active() => Ok(Some((id, owner))),
            Self::Ready { .. } => Err("The hydrated survivor renderer is no longer active".into()),
            Self::Opening => Err("The captured survivor is still opening".into()),
            Self::Pending(pending) => Err(pending.error.clone()),
            Self::Failed(error) => Err(error.clone()),
        }
    }
}

pub(super) struct GeneratedCleanupCoordinator {
    state: std::rc::Rc<TabsState>,
    authority: std::cell::RefCell<Option<GeneratedCleanupAuthority>>,
    request_participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    operation: u64,
    view: ViewId,
    binding: NativeBrowserBinding,
    actor: Option<std::rc::Rc<dyn crate::tabs::host::SpareActor>>,
    tab_id: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    spare: bool,
    close: Arc<BrowserClose>,
    hydrated: std::cell::RefCell<HydratedSurvivor>,
    prepared: std::cell::RefCell<Option<PreparedRemoval>>,
    claim: std::cell::RefCell<Option<NativeBrowserCloseClaim<()>>>,
    claimed: std::cell::Cell<bool>,
    registration: std::cell::RefCell<Option<PrecloseRegistration>>,
    retry: std::cell::RefCell<Option<crate::tabs::host::GeneratedPrecloseRetry>>,
    rollback_done: std::cell::Cell<bool>,
    phase: std::cell::RefCell<RemovalPhase>,
    #[allow(clippy::type_complexity)]
    waiter: std::cell::RefCell<Option<Box<dyn FnOnce(Result<(), String>)>>>,
}

impl GeneratedCleanupCoordinator {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare(
        state: std::rc::Rc<TabsState>,
        authority: GeneratedCleanupAuthority,
        operation: u64,
        view: ViewId,
        binding: NativeBrowserBinding,
        request_participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        actor: Option<std::rc::Rc<dyn crate::tabs::host::SpareActor>>,
        tab_id: std::rc::Rc<std::cell::RefCell<Option<String>>>,
        spare: bool,
        installer: crate::tabs::host::GeneratedPrecloseInstaller,
    ) -> Result<(std::rc::Rc<Self>, ViewCreationCleanup), (Box<GeneratedCleanupAuthority>, String)>
    {
        if authority.operation != operation
            || !authority.reservation.same_reservation(&binding.reservation)
            || !authority.browser.same_owner(&binding.owner)
            || !binding
                .owner
                .reservation()
                .same_reservation(&authority.reservation)
            || !authority
                .reservation
                .matches(view, authority.reservation.created())
        {
            return Err((
                Box::new(authority),
                "The generated cleanup operation or reservation changed".into(),
            ));
        }
        let (close, cleanup) = browser_cleanup(binding.clone());
        let coordinator = std::rc::Rc::new(Self {
            state: state.clone(),
            authority: std::cell::RefCell::new(Some(authority)),
            request_participants,
            operation,
            view,
            binding,
            actor,
            tab_id,
            spare,
            close,
            hydrated: std::cell::RefCell::new(HydratedSurvivor::Unstarted),
            prepared: std::cell::RefCell::new(None),
            claim: std::cell::RefCell::new(None),
            claimed: std::cell::Cell::new(false),
            registration: std::cell::RefCell::new(None),
            retry: std::cell::RefCell::new(None),
            rollback_done: std::cell::Cell::new(false),
            phase: std::cell::RefCell::new(RemovalPhase::Ready),
            waiter: std::cell::RefCell::new(None),
        });
        let barrier_owner = coordinator.clone();
        let failure_owner = coordinator.clone();
        let mailbox = preclose_mailbox(
            state.services.core.clone(),
            std::rc::Rc::new(move |done| barrier_owner.clone().preclose(done)),
            std::rc::Rc::new(move |error| {
                failure_owner.state.notify_pending_creation_error(
                    failure_owner.operation,
                    format!("Generated cleanup failed: {error}"),
                )
            }),
        );
        let (registration, barrier, failure) = match mailbox {
            Ok(mailbox) => mailbox,
            Err(error) => {
                let authority = coordinator
                    .authority
                    .borrow_mut()
                    .take()
                    .expect("preclose authority");
                return Err((Box::new(authority), error));
            }
        };
        match installer(barrier, failure) {
            Ok(retry) => {
                *coordinator.registration.borrow_mut() = Some(registration);
                *coordinator.retry.borrow_mut() = Some(retry);
                Ok((coordinator, cleanup))
            }
            Err(error) => {
                drop(registration);
                let authority = coordinator
                    .authority
                    .borrow_mut()
                    .take()
                    .expect("preclose authority");
                Err((Box::new(authority), error))
            }
        }
    }

    pub(super) fn rollback_store(
        &self,
    ) -> Result<crate::tabs::host::EditorSeedRollbackStore, String> {
        self.authority
            .borrow()
            .as_ref()
            .map(|authority| authority.rollback.clone())
            .ok_or_else(|| "The generated cleanup authority is unavailable".into())
    }

    pub(super) fn matches_binding(&self, binding: &NativeBrowserBinding) -> bool {
        self.binding
            .reservation
            .same_reservation(&binding.reservation)
            && self.binding.owner.same_owner(&binding.owner)
    }

    pub(super) fn activation_inputs(
        &self,
    ) -> Result<
        (
            TabActivationAuthority,
            Vec<shell_contract::ipc::NativeRequestOwner>,
            shell_contract::runtime::CancelToken,
            shell_contract::runtime::CancelToken,
        ),
        String,
    > {
        let authority = self.authority.borrow();
        let authority = authority
            .as_ref()
            .ok_or("The generated cleanup authority is unavailable")?;
        Ok((
            TabActivationAuthority::Interactive(authority.source.clone()),
            self.request_participants.clone(),
            authority.cancel.clone(),
            authority.pending.clone(),
        ))
    }

    pub(super) fn release(&self) {
        self.retry.borrow_mut().take();
        self.registration.borrow_mut().take();
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn rollback_action(
        self: &std::rc::Rc<Self>,
    ) -> std::rc::Rc<dyn Fn(Option<NativeBrowserRetirement>, Box<dyn FnOnce(Result<(), String>)>)>
    {
        let coordinator = self.clone();
        std::rc::Rc::new(move |proof, done| coordinator.clone().rollback(proof, done))
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn failed_lifetime_rollback(
        self: &std::rc::Rc<Self>,
        failure: Option<super::AdoptedLifetimeFailure>,
    ) -> std::rc::Rc<dyn Fn(Option<NativeBrowserRetirement>, Box<dyn FnOnce(Result<(), String>)>)>
    {
        match failure {
            None => self.rollback_action(),
            Some(super::AdoptedLifetimeFailure::CallerOwned(failure)) => {
                let custody = std::rc::Rc::new(std::cell::RefCell::new(Some(failure)));
                let coordinator = self.clone();
                std::rc::Rc::new(move |proof, done| {
                    let custody = custody.clone();
                    coordinator.clone().rollback(
                        proof,
                        Box::new(move |result| {
                            if result.is_ok() {
                                custody.borrow_mut().take();
                            }
                            done(result);
                        }),
                    );
                })
            }
            Some(super::AdoptedLifetimeFailure::Retained(failure)) => {
                let failure = std::rc::Rc::new(failure.into_handle());
                let coordinator = self.clone();
                std::rc::Rc::new(move |proof, done| {
                    let matches = match proof.as_ref() {
                        Some(NativeBrowserRetirement::Closed(receipt)) => {
                            receipt.matches(
                                &coordinator.binding.reservation,
                                coordinator.view,
                                coordinator.binding.reservation.created(),
                            ) && receipt
                                .terminal_owner()
                                .is_some_and(|owner| owner.same_owner(&coordinator.binding.owner))
                        }
                        Some(NativeBrowserRetirement::Superseded(proof)) => proof
                            .matches(&coordinator.binding.reservation, &coordinator.binding.owner),
                        None => false,
                    };
                    if !matches {
                        return done(Err(
                            "The retained generated lifetime has no matching native retirement"
                                .into(),
                        ));
                    }
                    let settled = coordinator.clone();
                    failure.observe_terminal(Box::new(move |terminal| {
                        let valid = match &terminal {
                            NativeBrowserRetirement::Closed(receipt) => {
                                receipt.matches(
                                    &settled.binding.reservation,
                                    settled.view,
                                    settled.binding.reservation.created(),
                                ) && receipt
                                    .terminal_owner()
                                    .is_some_and(|owner| owner.same_owner(&settled.binding.owner))
                            }
                            NativeBrowserRetirement::Superseded(proof) => {
                                proof.matches(&settled.binding.reservation, &settled.binding.owner)
                            }
                        };
                        if !valid {
                            return done(Err(
                                "The retained generated actor retired a different browser".into(),
                            ));
                        }
                        if let Some(authority) = settled.authority.borrow().as_ref() {
                            *authority.retirement.borrow_mut() = Some(terminal);
                        }
                        settled.rollback_done.set(true);
                        settled.finish_removal(done);
                    }));
                    if let Err(error) = failure.retry() {
                        coordinator
                            .state
                            .hooks
                            .warn("[shell] generated lifetime retirement is pending:", &error);
                    }
                })
            }
        }
    }

    fn rollback(
        self: std::rc::Rc<Self>,
        proof: Option<NativeBrowserRetirement>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let admitted = self.tab_id.borrow().is_some();
        let valid = match &proof {
            Some(NativeBrowserRetirement::Closed(receipt)) => {
                receipt.matches(
                    &self.binding.reservation,
                    self.view,
                    self.binding.reservation.created(),
                ) && receipt
                    .terminal_owner()
                    .is_some_and(|owner| owner.same_owner(&self.binding.owner))
            }
            Some(NativeBrowserRetirement::Superseded(proof)) => {
                proof.matches(&self.binding.reservation, &self.binding.owner)
            }
            None => !admitted,
        };
        if !valid {
            return done(Err(
                "The generated cleanup has no matching native retirement".into(),
            ));
        }
        let rollback = {
            let authority = self.authority.borrow();
            authority.as_ref().map(|authority| {
                *authority.retirement.borrow_mut() = proof.clone();
                authority.rollback.clone()
            })
        };
        let Some(rollback) = rollback else {
            return done(Err("The generated cleanup authority is unavailable".into()));
        };
        if self.rollback_done.get() {
            return self.finish_removal(done);
        }
        let coordinator = self.clone();
        let receipt = if admitted { proof } else { None };
        super::receipt_retirement::rollback_editor_receipts(
            rollback,
            receipt,
            admitted,
            Box::new(move |result| match result {
                Ok(()) => {
                    coordinator.rollback_done.set(true);
                    coordinator.finish_removal(done);
                }
                Err(error) => done(Err(error)),
            }),
        );
    }

    fn finish_removal(self: std::rc::Rc<Self>, done: Box<dyn FnOnce(Result<(), String>)>) {
        let Some(id) = self.tab_id.borrow().clone() else {
            self.release();
            return done(Ok(()));
        };
        let superseded = self
            .authority
            .borrow()
            .as_ref()
            .and_then(|authority| authority.retirement.borrow().clone())
            .is_some_and(|proof| matches!(proof, NativeBrowserRetirement::Superseded(_)));
        if superseded {
            return done(Err(
                "The generated browser was superseded before manager removal".into(),
            ));
        }
        enum Next {
            Start,
            Waiting,
            Settled(Result<(), String>),
        }
        let next = {
            let mut phase = self.phase.borrow_mut();
            match &*phase {
                RemovalPhase::Ready => {
                    *phase = RemovalPhase::Running;
                    Next::Start
                }
                RemovalPhase::Retryable | RemovalPhase::Running => Next::Waiting,
                RemovalPhase::Settled(result) => Next::Settled(result.clone()),
            }
        };
        match next {
            Next::Settled(result) => return done(result),
            Next::Waiting => return done(Err("The generated removal is already running".into())),
            Next::Start => {}
        }
        let Some(prepared) = self.prepared.borrow_mut().take() else {
            *self.phase.borrow_mut() = RemovalPhase::Ready;
            return done(Err("The prepared generated removal is unavailable".into()));
        };
        *self.waiter.borrow_mut() = Some(done);
        let PreparedRemoval {
            activation,
            removal,
            batch,
            reserved_revision,
        } = prepared;
        let activation = std::rc::Rc::new(std::cell::RefCell::new(activation));
        let revision_reservation = std::rc::Rc::new(std::cell::RefCell::new(reserved_revision));
        let commit_activation = activation.clone();
        let rejected_activation = activation;
        let commit_reservation = revision_reservation.clone();
        let rejected_reservation = revision_reservation;
        let effects_host = batch.effects_host.clone();
        let authority = batch.authority.clone();
        let participants = batch.participants.clone();
        let cancel = batch.cancel.clone();
        let pending = batch.pending.clone();
        let revision = batch.revision.clone();
        let rejected_removal = removal.clone();
        let coordinator = self.clone();
        let commit = Box::new(
            move |effects: &[main_core::shell::tab_manager::TabActivationEffect<
                ViewId,
                RetainedSpareBinding,
            >]| {
                let spare = effects.iter().find_map(|effect| match effect {
                    main_core::shell::tab_manager::TabActivationEffect::Close(_, binding) => {
                        Some(binding.clone())
                    }
                    _ => None,
                });
                let (_, close) = driver::commit_activation_with_spare(
                    &effects_host,
                    &authority,
                    &participants,
                    &cancel,
                    &pending,
                    spare,
                    || {
                        let authority = coordinator.authority.borrow();
                        let authority = authority
                            .as_ref()
                            .ok_or("The generated cleanup authority is unavailable")?;
                        if coordinator.state.manager_epoch.get() != authority.manager_epoch
                            || coordinator.state.shell_window() != Some(authority.shell_window)
                        {
                            return Err(
                                "The captured shell changed before generated removal".into()
                            );
                        }
                        let apply = || {
                            let mut inner = coordinator.state.inner.borrow_mut();
                            let manager = inner
                                .manager
                                .as_mut()
                                .ok_or("The shell window is unavailable")?;
                            manager.commit_prepared_generated_removal(
                                &id,
                                &coordinator.view,
                                &removal,
                                &mut *commit_activation.borrow_mut(),
                                |binding| {
                                    if coordinator.matches_binding(&binding.browser) {
                                        Ok(())
                                    } else {
                                        Err("The generated tab browser owner changed".into())
                                    }
                                },
                            )
                        };
                        let reserved = commit_reservation.borrow();
                        match reserved.as_ref() {
                            Some(reserved) => reserved.commit(apply),
                            None => revision
                                .with_current_commit(apply)
                                .ok_or("The split state changed before generated removal")?,
                        }
                    },
                )?;
                Ok(close)
            },
        );
        let rejected = self.clone();
        let on_rejected = Box::new(move |batch, _error| {
            let prepared = PreparedRemoval {
                activation: rejected_activation.borrow_mut().take(),
                removal: rejected_removal,
                batch,
                reserved_revision: rejected_reservation.borrow_mut().take(),
            };
            *rejected.prepared.borrow_mut() = Some(prepared);
            *rejected.phase.borrow_mut() = RemovalPhase::Ready;
        });
        let settled = self.clone();
        let on_settled = Box::new(move |result: Result<(), String>| {
            {
                let mut phase = settled.phase.borrow_mut();
                if !matches!(*phase, RemovalPhase::Ready) {
                    *phase = RemovalPhase::Settled(result.clone());
                }
            }
            if result.is_ok() {
                settled.release();
            }
            let done = settled.waiter.borrow_mut().take();
            if let Some(done) = done {
                done(result);
            }
        });
        let pending_owner = self.clone();
        let on_pending = std::rc::Rc::new(move |error: &str| {
            *pending_owner.phase.borrow_mut() = RemovalPhase::Retryable;
            pending_owner
                .state
                .notify_pending_creation_error(pending_owner.operation, error.to_owned());
            let waiters =
                std::mem::take(&mut *pending_owner.state.creation_cleanup_waiters.borrow_mut());
            for done in waiters {
                done(Err(error.to_owned()));
            }
        });
        let attempted = self.clone();
        let on_attempt = Box::new(move |result: Result<(), String>| {
            let running = matches!(*attempted.phase.borrow(), RemovalPhase::Running);
            if let (true, Err(error)) = (running, result) {
                *attempted.phase.borrow_mut() = RemovalPhase::Retryable;
                attempted
                    .state
                    .notify_pending_creation_error(attempted.operation, error.clone());
                let waiters =
                    std::mem::take(&mut *attempted.state.creation_cleanup_waiters.borrow_mut());
                for done in waiters {
                    done(Err(error.clone()));
                }
            }
        });
        self.state.enqueue_prepared_activation(
            batch,
            commit,
            on_rejected,
            Some(on_pending),
            on_attempt,
            on_settled,
        );
    }

    fn preclose(self: std::rc::Rc<Self>, done: PrecloseDone) {
        if self.claimed.get() {
            return done(Ok(()));
        }
        let id = self.tab_id.borrow().clone();
        let Some(id) = id else {
            if self.spare {
                return done(Err("The spare browser was not adopted".into()));
            }
            return done(self.claim(false));
        };
        if self.prepared.borrow().is_some() {
            return done(self.claim(true));
        }
        let hydration = {
            let authority = self.authority.borrow();
            authority
                .as_ref()
                .ok_or_else(|| "The generated cleanup authority is unavailable".to_owned())
                .and_then(survivor_to_hydrate)
        };
        let hydration = match hydration {
            Ok(hydration) => hydration,
            Err(error) => return done(Err(error)),
        };
        if let Some((opener, context)) = hydration {
            let pending = {
                let mut hydrated = self.hydrated.borrow_mut();
                match &*hydrated {
                    HydratedSurvivor::Pending(_) => Ok(Some(std::mem::replace(
                        &mut *hydrated,
                        HydratedSurvivor::Opening,
                    ))),
                    HydratedSurvivor::Opening => {
                        Err("The captured survivor is still opening".to_owned())
                    }
                    HydratedSurvivor::Failed(error) => Err(error.clone()),
                    _ => Ok(None),
                }
            };
            let pending = match pending {
                Ok(pending) => pending,
                Err(error) => return done(Err(error)),
            };
            if let Some(HydratedSurvivor::Pending(mut pending)) = pending {
                let resumed = pending.retry().and_then(|()| {
                    if !pending.generation.is_active() {
                        return Err("The hydrated survivor renderer is no longer active".into());
                    }
                    pending
                        .closure
                        .reservation
                        .with_owner(&pending.closure.owner, |_| Ok(()))
                });
                if let Err(error) = resumed {
                    pending.error = error.clone();
                    *self.hydrated.borrow_mut() = HydratedSurvivor::Pending(pending);
                    return done(Err(error));
                }
                let id = pending.id.clone();
                let owner = pending.owner.clone();
                let generation = pending.generation.clone();
                let retained = self
                    .authority
                    .borrow_mut()
                    .as_mut()
                    .map(|authority| authority.participants.push(owner.clone()))
                    .is_some();
                *self.hydrated.borrow_mut() = HydratedSurvivor::Ready {
                    id,
                    owner,
                    generation,
                };
                if !retained {
                    return done(Err("The generated cleanup authority is unavailable".into()));
                }
            }
            let needs_hydration = matches!(*self.hydrated.borrow(), HydratedSurvivor::Unstarted);
            if needs_hydration {
                let captured = {
                    let authority = self.authority.borrow();
                    authority
                        .as_ref()
                        .map(|authority| (authority.shell.clone(), authority.cleanup.clone()))
                };
                let Some((shell, cleanup)) = captured else {
                    return done(Err("The generated cleanup authority is unavailable".into()));
                };
                *self.hydrated.borrow_mut() = HydratedSurvivor::Opening;
                let coordinator = self.clone();
                let generated_id = id.clone();
                return self.state.hydrate_generated_survivor(
                    opener,
                    context,
                    shell.clone(),
                    shell,
                    cleanup.clone(),
                    cleanup,
                    Box::new(move |result| match result {
                        Ok(super::SurvivorHydration::Ready {
                            id,
                            owner,
                            generation,
                        }) => {
                            let retained = coordinator
                                .authority
                                .borrow_mut()
                                .as_mut()
                                .map(|authority| authority.participants.push(owner.clone()))
                                .is_some();
                            *coordinator.hydrated.borrow_mut() = HydratedSurvivor::Ready {
                                id,
                                owner,
                                generation,
                            };
                            if !retained {
                                return done(Err(
                                    "The generated cleanup authority is unavailable".into()
                                ));
                            }
                            done(coordinator.prepare_and_claim(&generated_id));
                        }
                        Ok(super::SurvivorHydration::AdoptedPending(pending)) => {
                            let error = pending.error.clone();
                            *coordinator.hydrated.borrow_mut() = HydratedSurvivor::Pending(pending);
                            done(Err(error));
                        }
                        Err(error) => {
                            *coordinator.hydrated.borrow_mut() =
                                HydratedSurvivor::Failed(error.clone());
                            done(Err(error));
                        }
                    }),
                );
            }
        }
        done(self.prepare_and_claim(&id));
    }

    fn prepare_and_claim(&self, id: &str) -> Result<(), String> {
        let preview = {
            let inner = self.state.inner.borrow();
            let manager = inner
                .manager
                .as_ref()
                .ok_or("The shell window is unavailable")?;
            manager.preview_closed_generated_owned(id, &self.view)?
        };
        let prepared = {
            let authority = self.authority.borrow();
            let authority = authority
                .as_ref()
                .ok_or("The generated cleanup authority is unavailable")?;
            let hydrated = self.hydrated.borrow();
            validate_survivors(authority, &preview, hydrated.ready_owner()?)?;
            prepare_removal(&self.state, authority, &preview)?
        };
        *self.prepared.borrow_mut() = Some(prepared);
        self.claim(true)
    }

    fn claim(&self, admitted: bool) -> Result<(), String> {
        if self.claimed.get() {
            return Ok(());
        }
        if admitted {
            let id = self
                .tab_id
                .borrow()
                .clone()
                .ok_or("The generated tab identity is unavailable")?;
            let authority = self.authority.borrow();
            let authority = authority
                .as_ref()
                .ok_or("The generated cleanup authority is unavailable")?;
            if self.state.manager_epoch.get() != authority.manager_epoch
                || self.state.shell_window() != Some(authority.shell_window)
            {
                return Err("The captured shell changed before generated close".into());
            }
            let inner = self.state.inner.borrow();
            let manager = inner
                .manager
                .as_ref()
                .ok_or("The shell window is unavailable")?;
            let preview = manager.preview_closed_generated_owned(&id, &self.view)?;
            let hydrated = self.hydrated.borrow();
            validate_survivors(authority, &preview, hydrated.ready_owner()?)?;
            let prepared = self.prepared.borrow();
            let prepared = prepared
                .as_ref()
                .ok_or("The prepared generated removal is unavailable")?;
            if preview.removal() != &prepared.removal {
                return Err("The generated removal state changed".into());
            }
            preview.manager().validate_loaded_activation(
                prepared
                    .activation
                    .as_ref()
                    .ok_or("The generated removal was already committed")?,
            )?;
        }
        self.close.arm_watch()?;
        if self.close.has_retirement() {
            self.claimed.set(true);
            return Ok(());
        }
        let pending_claim = self.claim.borrow_mut().take();
        let claim = match pending_claim {
            Some(claim) => claim,
            None => {
                let claimed = if admitted {
                    let prepared = self.prepared.borrow();
                    let batch = &prepared
                        .as_ref()
                        .ok_or("The prepared generated removal is unavailable")?
                        .batch;
                    driver::admit(
                        &batch.effects_host,
                        &batch.authority,
                        &batch.participants,
                        &batch.cancel,
                        &batch.pending,
                        || claim_browser_close(&self.binding, self.actor.as_ref()),
                    )
                } else {
                    claim_browser_close(&self.binding, None)
                };
                match claimed {
                    Ok(claim) => claim,
                    Err(_error) if self.close.has_retirement() => {
                        self.claimed.set(true);
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        match self.close.install_claim(claim) {
            Ok(()) => {
                self.claimed.set(true);
                Ok(())
            }
            Err((error, claim)) => {
                *self.claim.borrow_mut() = Some(claim);
                Err(error)
            }
        }
    }
}
