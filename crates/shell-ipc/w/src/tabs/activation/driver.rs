use super::super::{
    host::{RetainedSpareBinding, TabsChanged, TimerKey},
    state::{Inner, TabsState, split_shape},
};
use super::{
    PreparedTabActivationPublication, PreparedTabUiStep, TabActivationAuthority,
    TabActivationPublicationSnapshot, TabUiDone, TabUiStepDone, TabUiStepOutcome,
};
use main_core::shell::tab_manager::TabActivationEffect;
use shell_contract::{
    ViewId, hosts::OwnedEffectsHost, ipc::NativeRequestOwner, runtime::CancelToken,
};
use std::{collections::VecDeque, rc::Rc, sync::Arc};

pub(crate) fn capture_shell_owner(
    state: &TabsState,
    effects: &OwnedEffectsHost,
    shell: super::super::host::ShellWindow,
) -> Result<NativeRequestOwner, String> {
    let generation = state
        .services
        .views
        .renderer_generation(shell.view)
        .ok_or("The captured shell renderer is unavailable")?;
    effects.capture_owner(shell.view, &generation)
}

pub(crate) fn admit<T>(
    effects: &OwnedEffectsHost,
    authority: &TabActivationAuthority,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let mut action = Some(f);
    let mut result = None;
    let mut apply = || {
        result = Some(action
            .take()
            .ok_or("The activation was already admitted")?(
        )?);
        Ok(())
    };
    match authority {
        TabActivationAuthority::Interactive(origin) => {
            effects.with_owned_effect_many(origin, participants, cancel, pending, &mut apply)?
        }
        TabActivationAuthority::NativeInvocation { document, owner } => effects
            .with_native_invocation_effect(
                document,
                owner,
                participants,
                cancel,
                pending,
                &mut apply,
            )?,
        TabActivationAuthority::NativeOperation { operation, owner } => effects
            .with_native_operation_effect(
                operation,
                owner,
                participants,
                cancel,
                pending,
                &mut apply,
            )?,
        TabActivationAuthority::StartupRecovery {
            startup,
            shell,
            child,
            owner,
        } => effects.with_startup_recovery_effect(
            startup,
            shell,
            child,
            owner,
            participants,
            cancel,
            pending,
            &mut apply,
        )?,
        TabActivationAuthority::NativeCommand {
            operation,
            expected,
            owner,
        } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            effects.with_native_command_preparation_effect(
                operation, expected, &captured, cancel, pending, &mut apply,
            )?;
        }
        TabActivationAuthority::NativeCommandTarget {
            document,
            expected,
            owner,
        } => effects.with_native_command_target_effect(
            document.operation(),
            expected,
            document,
            owner,
            participants,
            cancel,
            pending,
            &mut apply,
        )?,
        TabActivationAuthority::StartupCommand {
            startup,
            shell,
            child,
            expected,
            owner,
        } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            effects.with_startup_command_preparation_effect(
                startup, shell, child, expected, &captured, cancel, pending, &mut apply,
            )?;
        }
        TabActivationAuthority::StartupCommandTarget {
            startup,
            shell,
            document,
            expected,
            owner,
        } => effects.with_startup_command_target_effect(
            startup,
            shell,
            document.operation(),
            expected,
            document,
            owner,
            participants,
            cancel,
            pending,
            &mut apply,
        )?,
        TabActivationAuthority::CommandRecovery {
            command,
            child,
            owner,
        } => effects.with_command_recovery_target_effect(
            command,
            child,
            owner,
            participants,
            cancel,
            pending,
            &mut apply,
        )?,
        TabActivationAuthority::ExistingDocumentSave {
            path,
            owner,
            additional,
        } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            path.with_effect(effects, &captured, cancel, pending, additional, &mut apply)?;
        }
        TabActivationAuthority::SavedDocument {
            document,
            owner,
            additional,
        } => {
            let extra: Vec<_> = additional.iter().collect();
            effects.with_native_saved_document_same_origin(
                document,
                owner,
                cancel,
                participants,
                cancel,
                pending,
                &extra,
                &mut |_| apply(),
            )?;
        }
        TabActivationAuthority::NativeSaveReload { receipt, owner } => effects
            .with_native_save_reload_many_effect(
                receipt,
                owner,
                participants,
                cancel,
                pending,
                &mut apply,
            )?,
    }
    result.ok_or_else(|| "The activation was not admitted".into())
}

fn validate_native_batch(
    effects: &[TabActivationEffect<ViewId, RetainedSpareBinding>],
    steps: &[Option<PreparedTabUiStep>],
) -> Result<(), String> {
    if effects.len() != steps.len() {
        return Err("The captured activation batch is incomplete".into());
    }
    for (index, (effect, step)) in effects.iter().zip(steps).enumerate() {
        let native = matches!(
            effect,
            TabActivationEffect::RemoveChild(_, _)
                | TabActivationEffect::Close(_, _)
                | TabActivationEffect::Visible(_, _)
                | TabActivationEffect::Bounds(_, _)
                | TabActivationEffect::Menu(_)
                | TabActivationEffect::Focus(_)
                | TabActivationEffect::FocusShell
        );
        if native != step.is_some() {
            return Err("The captured activation step is unavailable".into());
        }
        if let TabActivationEffect::RemoveChild(view, binding) = effect
            && !matches!(effects.get(index + 1), Some(TabActivationEffect::Close(next, captured)) if next == view && captured == binding)
        {
            return Err("The spare removal does not match its close".into());
        }
        if let TabActivationEffect::Close(view, binding) = effect
            && (index == 0
                || !matches!(effects.get(index - 1), Some(TabActivationEffect::RemoveChild(previous, captured)) if previous == view && captured == binding))
        {
            return Err("The spare close does not match its removal".into());
        }
    }
    Ok(())
}

pub(crate) enum CommittedSpareClose {
    None,
    Won(shell_contract::ipc::NativeBrowserClosePermit),
    Superseded(
        Box<RetainedSpareBinding>,
        shell_contract::ipc::NativeBrowserSuperseded,
    ),
}

pub(crate) struct PreparedCommittedActivationBatch {
    pub authority: TabActivationAuthority,
    pub participants: Vec<NativeRequestOwner>,
    pub base_count: usize,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub effects_host: OwnedEffectsHost,
    pub effects: Vec<TabActivationEffect<ViewId, RetainedSpareBinding>>,
    pub native: super::PreparedTabUiBatch,
    pub publication: Option<PreparedTabActivationPublication>,
    pub revision: super::SplitMemoryRevision,
}

pub(in super::super) struct PreparedActivationPresentation {
    pub participants: Vec<NativeRequestOwner>,
    pub base_count: usize,
    pub effects_host: OwnedEffectsHost,
    pub native: super::PreparedTabUiBatch,
    pub publication: Option<PreparedTabActivationPublication>,
    pub revision: super::SplitMemoryRevision,
    pub reserved_revision: Option<super::ReservedSplitMemoryRevision>,
}

pub(in super::super) enum ActivationParticipants {
    CaptureCurrent(Vec<NativeRequestOwner>),
    CapturedOnly(Vec<NativeRequestOwner>),
}

#[allow(clippy::too_many_arguments)]
pub(in super::super) fn prepare_activation_presentation(
    state: &TabsState,
    effects_host: OwnedEffectsHost,
    preview: &main_core::shell::tab_manager::TabManager<ViewId, RetainedSpareBinding>,
    activation_effects: &[TabActivationEffect<ViewId, RetainedSpareBinding>],
    shell: super::super::host::ShellWindow,
    shell_owner: &NativeRequestOwner,
    expected_view: Option<ViewId>,
    authority: &TabActivationAuthority,
    participants: ActivationParticipants,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<PreparedActivationPresentation, String> {
    let (mut participants, capture_missing) = match participants {
        ActivationParticipants::CaptureCurrent(participants) => (participants, true),
        ActivationParticipants::CapturedOnly(participants) => (participants, false),
    };
    let mut native =
        state
            .hooks
            .prepare_activation_ui(shell, shell_owner, expected_view, activation_effects)?;
    validate_native_batch(activation_effects, &native.steps)?;
    participants.push(native.shell.clone());
    let base_count = participants.len();
    participants.append(&mut native.participants);
    let (reserved_revision, revision) = {
        let inner = state.inner.borrow();
        let reserved = if activation_effects
            .iter()
            .any(|effect| matches!(effect, TabActivationEffect::SplitChanged(_)))
        {
            Some(
                inner
                    .split_memory_clock
                    .reserve_advance()
                    .ok_or("The split memory revision is unavailable")?,
            )
        } else {
            None
        };
        let revision = match &reserved {
            Some(reserved) => reserved.revision(),
            None => inner
                .split_memory_clock
                .capture()
                .ok_or("The split memory revision is unavailable")?,
        };
        (reserved, revision)
    };
    let needs_publication = activation_effects.iter().any(|step| {
        matches!(
            step,
            TabActivationEffect::Changed | TabActivationEffect::SplitChanged(_)
        )
    });
    let publication = if needs_publication {
        let (changed, settings_path) = {
            let mut inner = state.inner.borrow_mut();
            let Inner {
                detached, bridge, ..
            } = &mut *inner;
            (
                TabsChanged {
                    tabs: preview.list(),
                    open_paths: preview.open_file_paths(),
                    detached_paths: detached.detached_file_paths(bridge),
                },
                bridge.settings_path.clone(),
            )
        };
        let (_, session_detached) = state.session_snapshot()?;
        let session_strip = preview.session_tabs().to_value();
        let mut tab_views = Vec::new();
        for tab in &changed.tabs {
            if let Some(view) = preview.web_contents_for_tab(&tab.id) {
                tab_views.push((tab.id.clone(), view));
            }
        }
        for row in &session_detached {
            if let Some(id) = row.get("id").and_then(serde_json::Value::as_str)
                && let Some(view) = state.tab_contents(id)
            {
                tab_views.push((id.to_owned(), view));
            }
        }
        for (_, view) in &tab_views {
            if !participants.iter().any(|owner| owner.view == *view)
                && authority.owner().view != *view
            {
                if !capture_missing {
                    return Err("The activation publication contains an uncaptured tab".into());
                }
                let generation = state
                    .services
                    .views
                    .renderer_generation(*view)
                    .ok_or("A captured tab renderer is unavailable")?;
                participants.push(effects_host.capture_owner(*view, &generation)?);
            }
        }
        let snapshot = TabActivationPublicationSnapshot {
            changed,
            tab_views,
            split: preview.split_state(),
            settings_path,
            session_strip,
            session_detached,
            split_memory_revision: revision.clone(),
        };
        let closing: Vec<_> = activation_effects
            .iter()
            .filter_map(|step| match step {
                TabActivationEffect::Close(view, _) => Some(*view),
                _ => None,
            })
            .collect();
        let publication_scope = participants
            .iter()
            .enumerate()
            .filter(|(index, owner)| *index < base_count || !closing.contains(&owner.view))
            .map(|(_, owner)| owner.clone())
            .collect();
        Some(state.hooks.prepare_activation_publication_owned(
            authority.clone(),
            native.shell.clone(),
            publication_scope,
            cancel.clone(),
            pending.clone(),
            effects_host.clone(),
            snapshot,
        )?)
    } else {
        None
    };
    Ok(PreparedActivationPresentation {
        participants,
        base_count,
        effects_host,
        native,
        publication,
        revision,
        reserved_revision,
    })
}

pub(in super::super) fn commit_activation_with_spare<T>(
    effects: &OwnedEffectsHost,
    authority: &TabActivationAuthority,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    spare: Option<RetainedSpareBinding>,
    commit: impl FnOnce() -> Result<T, String>,
) -> Result<(T, CommittedSpareClose), String> {
    let actor = spare
        .as_ref()
        .map(|binding| {
            binding
                .spare_actor
                .clone()
                .ok_or("The captured spare actor is unavailable")
        })
        .transpose()?;
    let mut close_plan = actor
        .as_ref()
        .map(|actor| actor.prepare_close())
        .transpose()?;
    admit(effects, authority, participants, cancel, pending, || {
        let mut commit = Some(commit);
        let mut apply = || {
            commit
                .take()
                .ok_or("The activation was already committed")?()
        };
        if let Some(binding) = spare {
            let plan = close_plan
                .as_mut()
                .ok_or("The captured spare close plan is unavailable")?;
            let mut committed = None;
            let claim =
                binding
                    .browser
                    .reservation
                    .claim_close(&binding.browser.owner, |transition| {
                        plan.with_validated(transition, &mut || {
                            committed = Some(apply()?);
                            Ok(())
                        })
                    })?;
            match claim {
                shell_contract::ipc::NativeBrowserCloseClaim::Won { permit, .. } => Ok((
                    committed.ok_or("The spare close did not commit activation")?,
                    CommittedSpareClose::Won(permit),
                )),
                shell_contract::ipc::NativeBrowserCloseClaim::Superseded(proof) => Ok((
                    apply()?,
                    CommittedSpareClose::Superseded(Box::new(binding), proof),
                )),
            }
        } else {
            Ok((apply()?, CommittedSpareClose::None))
        }
    })
}

pub(crate) type CommitPreparedActivation = Box<
    dyn FnOnce(
        &[TabActivationEffect<ViewId, RetainedSpareBinding>],
    ) -> Result<CommittedSpareClose, String>,
>;

struct Run {
    state: Rc<TabsState>,
    authority: TabActivationAuthority,
    participants: Vec<NativeRequestOwner>,
    base_count: usize,
    cancel: CancelToken,
    pending: CancelToken,
    effects: OwnedEffectsHost,
    steps: VecDeque<(
        TabActivationEffect<ViewId, RetainedSpareBinding>,
        Option<PreparedTabUiStep>,
    )>,
    close: Option<shell_contract::ipc::NativeBrowserClosePermit>,
    superseded: Option<(
        RetainedSpareBinding,
        shell_contract::ipc::NativeBrowserSuperseded,
    )>,
    restored: Option<RetainedSpareBinding>,
    changed: Option<Box<dyn FnOnce(TabUiStepDone)>>,
    split: Option<Box<dyn FnOnce(bool, TabUiStepDone)>>,
    persist_split: Option<Box<dyn FnOnce(TabUiStepDone)>>,
    done: Option<TabUiDone>,
    settled: Option<TabUiDone>,
    #[allow(clippy::type_complexity)]
    pending_failure: Option<Rc<dyn Fn(&str)>>,
    had_pending: bool,
    revision: super::SplitMemoryRevision,
}

impl Run {
    fn finish(self, result: Result<(), String>) {
        if self.had_pending && result.is_ok() {
            self.state.activation_retry_error.borrow_mut().take();
        }
        let next = self.state.activation_queue.borrow_mut().pop_front();
        if next.is_none() {
            self.state.activation_running.set(false);
        }
        if let Some(settled) = self.settled {
            settled(result.clone());
        }
        if let Some(done) = self.done {
            done(result);
        }
        self.state.finish_creation_drain_if_ready();
        if let Some(next) = next {
            next();
        }
    }

    fn complete_step(
        mut self,
        result: Result<TabUiStepOutcome, String>,
        next: Box<dyn FnOnce(Self)>,
    ) {
        match result {
            Ok(TabUiStepOutcome::Applied) => next(self),
            Ok(TabUiStepOutcome::AppliedWithClosePermit(permit)) => {
                if self.close.is_some() {
                    return self
                        .finish(Err("The activation already retained a close permit".into()));
                }
                self.close = Some(permit);
                next(self);
            }
            Ok(TabUiStepOutcome::Superseded(_)) | Ok(TabUiStepOutcome::BrowserSuperseded(_)) => {
                self.finish(Err("The captured activation step was superseded".into()))
            }
            Ok(TabUiStepOutcome::CommittedCleanupPending { error, retry }) => {
                self.had_pending = true;
                let done = self.done.take();
                let pending_failure = self.pending_failure.clone();
                let state = self.state.clone();
                *state.activation_retry_error.borrow_mut() = Some(error.clone());
                *state.activation_retry.borrow_mut() = Some(Box::new(move || {
                    retry(Box::new(move |result| self.complete_step(result, next)));
                }));
                state.finish_creation_drain_if_ready();
                if let Some(pending_failure) = pending_failure {
                    pending_failure(&error);
                }
                if let Some(done) = done {
                    done(Err(error));
                }
            }
            Err(error) => self.finish(Err(error)),
        }
    }

    fn step_result(
        self,
        result: Result<TabUiStepOutcome, String>,
        closed: Option<ViewId>,
        returns_close: bool,
    ) {
        self.complete_step(
            result,
            Box::new(move |mut run| {
                if returns_close && run.close.is_none() {
                    return run
                        .finish(Err("The native move did not return its close permit".into()));
                }
                if let Some(view) = closed
                    && !run
                        .steps
                        .iter()
                        .any(|(effect, _)| effect.view() == Some(&view))
                {
                    let mut index = run.participants.len();
                    while index > run.base_count {
                        index -= 1;
                        if run.participants[index].view == view {
                            run.participants.remove(index);
                        }
                    }
                }
                run.advance();
            }),
        );
    }

    fn advance(mut self) {
        if let Some(binding) = self.restored.take() {
            self.steps.retain(|(effect, _)| !matches!(effect,
                TabActivationEffect::RemoveChild(_, captured) | TabActivationEffect::Close(_, captured) if captured == &binding));
        }
        if let Some((binding, proof)) = self.superseded.take() {
            if let Err(error) = self.state.supersede_adopted_editor(
                &binding.browser.reservation,
                &binding.browser.owner,
                proof,
            ) {
                return self.finish(Err(error));
            }
            if let super::super::host::NativeBrowserRegistration::Registered(owner) =
                &binding.browser.registration
                && let Some(loss) = owner.loss_token()
            {
                let mut index = self.participants.len();
                while index > self.base_count {
                    index -= 1;
                    if self.participants[index]
                        .loss_token()
                        .is_some_and(|candidate| candidate.same_token(&loss))
                    {
                        self.participants.remove(index);
                    }
                }
            }
            self.steps.retain(|(effect, _)| !matches!(effect,
                TabActivationEffect::RemoveChild(_, captured) | TabActivationEffect::Close(_, captured) if captured == &binding));
        }
        while let Some((effect, native)) = self.steps.pop_front() {
            if let Some(native) = native {
                let closed = match &effect {
                    TabActivationEffect::Close(view, _) => Some(*view),
                    _ => None,
                };
                let returns_close = matches!(&effect, TabActivationEffect::RemoveChild(_, _));
                let participants: Arc<[NativeRequestOwner]> = self.participants.clone().into();
                let authority = self.authority.clone();
                let cancel = self.cancel.clone();
                let pending = self.pending.clone();
                let close = if closed.is_some() || returns_close {
                    match self.close.take() {
                        Some(permit) => Some(permit),
                        None => {
                            return self
                                .finish(Err("The native close permit is unavailable".into()));
                        }
                    }
                } else {
                    None
                };
                native.run(
                    authority,
                    participants,
                    cancel,
                    pending,
                    close,
                    Box::new(move |result| {
                        self.step_result(result, closed, returns_close);
                    }),
                );
                return;
            }
            match effect {
                TabActivationEffect::Changed => {
                    let Some(changed) = self.changed.take() else {
                        return self
                            .finish(Err("The activation publication is unavailable".into()));
                    };
                    changed(Box::new(move |result| {
                        self.complete_step(result, Box::new(|run| run.advance()))
                    }));
                    return;
                }
                TabActivationEffect::SplitChanged(split) => {
                    if self.split.is_none() || self.persist_split.is_none() {
                        return self
                            .finish(Err("The activation publication is unavailable".into()));
                    }
                    let apply = admit(
                        &self.effects,
                        &self.authority,
                        &self.participants,
                        &self.cancel,
                        &self.pending,
                        || {
                            let mut inner = self.state.inner.borrow_mut();
                            let ratio = split.is_some()
                                && inner.last_split_state.is_some()
                                && split_shape(&split) == split_shape(&inner.last_split_state);
                            inner.last_split_state = split;
                            Ok((ratio, inner.split_timer))
                        },
                    );
                    let (ratio, had_timer) = match apply {
                        Ok(value) => value,
                        Err(error) => return self.finish(Err(error)),
                    };
                    let Some(persist) = self.persist_split.take() else {
                        return self.finish(Err("Split memory was already scheduled".into()));
                    };
                    let Some(publish) = self.split.take() else {
                        return self.finish(Err("Split state was already published".into()));
                    };
                    publish(
                        ratio,
                        Box::new(move |result| {
                            self.complete_step(
                                result,
                                Box::new(move |run| {
                                    if run.revision.with_current_commit(|| ()).is_none() {
                                        return run.finish(Err(
                                            "The split state changed before publication completed"
                                                .into(),
                                        ));
                                    }
                                    if had_timer {
                                        run.state.windows.clear_timer(TimerKey::SplitMemory);
                                    }
                                    if ratio {
                                        let mut inner = run.state.inner.borrow_mut();
                                        inner.pending_owned_split_memory = Some(persist);
                                        inner.split_timer = true;
                                        drop(inner);
                                        run.state.windows.set_timer(TimerKey::SplitMemory, 400.0);
                                        run.advance();
                                    } else {
                                        run.state.inner.borrow_mut().split_timer = false;
                                        persist(Box::new(move |result| {
                                            run.complete_step(result, Box::new(|run| run.advance()))
                                        }));
                                    }
                                }),
                            )
                        }),
                    );
                    return;
                }
                TabActivationEffect::SetSpareTimer(delay) => {
                    if let Err(error) = admit(
                        &self.effects,
                        &self.authority,
                        &self.participants,
                        &self.cancel,
                        &self.pending,
                        || Ok(()),
                    ) {
                        return self.finish(Err(error));
                    }
                    self.state.windows.set_timer(TimerKey::SpareSheets, delay);
                }
                TabActivationEffect::ClearSpareTimer => {
                    if let Err(error) = admit(
                        &self.effects,
                        &self.authority,
                        &self.participants,
                        &self.cancel,
                        &self.pending,
                        || Ok(()),
                    ) {
                        return self.finish(Err(error));
                    }
                    self.state.windows.clear_timer(TimerKey::SpareSheets);
                }
                TabActivationEffect::ActiveDocs(_)
                | TabActivationEffect::ActiveSheets(_)
                | TabActivationEffect::ActiveSlides(_) => {
                    let result = admit(
                        &self.effects,
                        &self.authority,
                        &self.participants,
                        &self.cancel,
                        &self.pending,
                        || {
                            effect.apply(&mut self.state.inner.borrow_mut().bridge);
                            Ok(())
                        },
                    );
                    if let Err(error) = result {
                        return self.finish(Err(error));
                    }
                }
                _ => {
                    return self.finish(Err("A captured native activation step is missing".into()));
                }
            }
        }
        self.finish(Ok(()));
    }
}

enum Plan {
    Activate(String),
    Split {
        a: String,
        b: String,
        a_view: ViewId,
        b_view: ViewId,
        orientation: main_core::shell::split_layout::SplitOrientation,
        ratio: f64,
        focus: main_core::shell::split_layout::Pane,
    },
    Layout,
    Restore,
    Bleed {
        view: ViewId,
        on: bool,
    },
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn start(
    state: Rc<TabsState>,
    plan: Plan,
    authority: TabActivationAuthority,
    target: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    done: TabUiDone,
    pending_failure: Option<Rc<dyn Fn(&str)>>,
    settled: Option<TabUiDone>,
) {
    let mut done = Some(done);
    let mut settled = settled;
    let mut pending_failure = pending_failure;
    let build = (|| -> Result<Option<Run>, String> {
        let effects = state
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or("Owned activation is unavailable")?;
        let (prepared, shell, expected_view) = {
            let mut inner = state.inner.borrow_mut();
            let Inner {
                manager, bridge, ..
            } = &mut *inner;
            let manager = manager.as_mut().ok_or("The shell window is unavailable")?;
            let shell = bridge.shell.ok_or("The shell window is unavailable")?;
            let active = manager
                .list()
                .into_iter()
                .find(|tab| tab.active)
                .ok_or("The active tab is unavailable")?;
            let id = match &plan {
                Plan::Activate(id) => id.as_str(),
                Plan::Split { a, b, focus, .. } => match focus {
                    main_core::shell::split_layout::Pane::A => a,
                    main_core::shell::split_layout::Pane::B => b,
                },
                _ => active.id.as_str(),
            };
            let expected = manager.web_contents_for_tab(id);
            if matches!(&plan, Plan::Activate(_) | Plan::Split { .. })
                && expected.unwrap_or(shell.view) != target.view
            {
                return Err("The captured tab no longer matches its document".into());
            }
            let env = manager.activation_environment(bridge, id);
            let prepared = match &plan {
                Plan::Activate(id) => Some(manager.prepare_loaded_activation(id, env)?),
                Plan::Split {
                    a,
                    b,
                    a_view,
                    b_view,
                    orientation,
                    ratio,
                    focus,
                } => {
                    if manager.web_contents_for_tab(a) != Some(*a_view)
                        || manager.web_contents_for_tab(b) != Some(*b_view)
                    {
                        return Err("A captured split pane changed".into());
                    }
                    manager.prepare_loaded_split(a, b, *orientation, *ratio, *focus, env)?
                }
                Plan::Layout => Some(manager.prepare_current_layout(env)),
                Plan::Restore => Some(manager.prepare_prompt_restore(env)),
                Plan::Bleed { view, on } => Some(manager.prepare_content_bleed(
                    i64::from(view.0),
                    *on,
                    env,
                    expected.map(|v| i64::from(v.0)),
                )),
            };
            (prepared, shell, expected)
        };
        let Some(prepared) = prepared else {
            admit(
                &effects,
                &authority,
                &participants,
                &cancel,
                &pending,
                || Ok(()),
            )?;
            return Ok(None);
        };
        let shell_owner = capture_shell_owner(&state, &effects, shell)?;
        let preview = {
            let inner = state.inner.borrow();
            let manager = inner
                .manager
                .as_ref()
                .ok_or("The shell window is unavailable")?;
            manager.preview_loaded_activation(&prepared)?
        };
        let mut captured = participants;
        captured.push(target);
        let PreparedActivationPresentation {
            participants: captured,
            base_count,
            effects_host: effects,
            native,
            publication,
            revision,
            reserved_revision,
        } = prepare_activation_presentation(
            &state,
            effects,
            &preview,
            prepared.effects(),
            shell,
            &shell_owner,
            expected_view,
            &authority,
            ActivationParticipants::CaptureCurrent(captured),
            &cancel,
            &pending,
        )?;
        let spare = prepared.effects().iter().find_map(|effect| match effect {
            TabActivationEffect::Close(_, binding) => Some(binding.clone()),
            _ => None,
        });
        let mut prepared = Some(prepared);
        let (steps, close) = commit_activation_with_spare(
            &effects,
            &authority,
            &captured,
            &cancel,
            &pending,
            spare,
            || {
                let mut inner = state.inner.borrow_mut();
                let manager = inner
                    .manager
                    .as_mut()
                    .ok_or("The shell window is unavailable")?;
                manager.validate_loaded_activation(
                    prepared
                        .as_ref()
                        .ok_or("The activation was already committed")?,
                )?;
                let apply = || {
                    manager.commit_loaded_activation(
                        prepared
                            .take()
                            .ok_or("The activation was already committed")?,
                    )
                };
                match &reserved_revision {
                    Some(reserved) => reserved.commit(apply),
                    None => revision
                        .with_current_commit(apply)
                        .ok_or("The split state changed before activation")?,
                }
            },
        )?;
        let (close, superseded, restored) = match close {
            CommittedSpareClose::None => (None, None, None),
            CommittedSpareClose::Won(permit) => (Some(permit), None, None),
            CommittedSpareClose::Superseded(binding, proof) => {
                (None, Some((*binding, proof)), None)
            }
        };
        let (changed, split, persist_split) = match publication {
            Some(PreparedTabActivationPublication {
                changed,
                split,
                persist_split,
            }) => (Some(changed), Some(split), Some(persist_split)),
            None => (None, None, None),
        };
        Ok(Some(Run {
            state: state.clone(),
            authority,
            participants: captured,
            base_count,
            cancel,
            pending,
            effects,
            steps: steps.into_iter().zip(native.steps).collect(),
            close,
            superseded,
            restored,
            changed,
            split,
            persist_split,
            revision,
            done: Some(
                done.take()
                    .ok_or("The activation completion was already consumed")?,
            ),
            settled: settled.take(),
            pending_failure: pending_failure.take(),
            had_pending: false,
        }))
    })();
    match build {
        Ok(Some(run)) => run.advance(),
        Ok(None) => {
            let next = state.activation_queue.borrow_mut().pop_front();
            if next.is_none() {
                state.activation_running.set(false);
            }
            if let Some(settled) = settled.take() {
                settled(Ok(()));
            }
            if let Some(done) = done.take() {
                done(Ok(()));
            }
            state.finish_creation_drain_if_ready();
            if let Some(next) = next {
                next();
            }
        }
        Err(error) => {
            let next = state.activation_queue.borrow_mut().pop_front();
            if next.is_none() {
                state.activation_running.set(false);
            }
            if let Some(settled) = settled.take() {
                settled(Err(error.clone()));
            }
            if let Some(done) = done.take() {
                done(Err(error));
            }
            state.finish_creation_drain_if_ready();
            if let Some(next) = next {
                next();
            }
        }
    }
}

impl TabsState {
    #[allow(clippy::type_complexity)]
    pub(crate) fn enqueue_prepared_activation(
        &self,
        batch: PreparedCommittedActivationBatch,
        commit: CommitPreparedActivation,
        rejected: Box<dyn FnOnce(PreparedCommittedActivationBatch, String)>,
        pending_failure: Option<Rc<dyn Fn(&str)>>,
        done: TabUiDone,
        settled: TabUiDone,
    ) {
        let weak = self.this.clone();
        let action: Box<dyn FnOnce()> = Box::new(move || {
            let Some(state) = weak.upgrade() else {
                let error = "The tab manager is unavailable".to_owned();
                rejected(batch, error.clone());
                settled(Err(error.clone()));
                return done(Err(error));
            };
            let result = if batch.base_count > batch.participants.len() {
                Err("The captured activation batch is incomplete".into())
            } else {
                validate_native_batch(&batch.effects, &batch.native.steps)
                    .and_then(|()| commit(&batch.effects))
            };
            let close = match result {
                Ok(close) => close,
                Err(error) => {
                    let next = state.activation_queue.borrow_mut().pop_front();
                    if next.is_none() {
                        state.activation_running.set(false);
                    }
                    rejected(batch, error.clone());
                    settled(Err(error.clone()));
                    done(Err(error));
                    state.finish_creation_drain_if_ready();
                    if let Some(next) = next {
                        next();
                    }
                    return;
                }
            };
            let (close, superseded, restored) = match close {
                CommittedSpareClose::None => (None, None, None),
                CommittedSpareClose::Won(permit) => (Some(permit), None, None),
                CommittedSpareClose::Superseded(binding, proof) => {
                    (None, Some((*binding, proof)), None)
                }
            };
            let (changed, split, persist_split) = match batch.publication {
                Some(publication) => (
                    Some(publication.changed),
                    Some(publication.split),
                    Some(publication.persist_split),
                ),
                None => (None, None, None),
            };
            Run {
                state,
                authority: batch.authority,
                participants: batch.participants,
                base_count: batch.base_count,
                cancel: batch.cancel,
                pending: batch.pending,
                effects: batch.effects_host,
                steps: batch.effects.into_iter().zip(batch.native.steps).collect(),
                close,
                superseded,
                restored,
                changed,
                split,
                persist_split,
                done: Some(done),
                settled: Some(settled),
                pending_failure,
                had_pending: false,
                revision: batch.revision,
            }
            .advance();
        });
        if self.activation_running.replace(true) {
            self.activation_queue.borrow_mut().push_back(action);
        } else {
            action();
        }
    }

    pub fn retry_activation_cleanup(&self) -> bool {
        let retry = self.activation_retry.borrow_mut().take();
        if let Some(retry) = retry {
            retry();
            true
        } else {
            false
        }
    }

    pub(crate) fn finish_split_persist(&self, result: Result<TabUiStepOutcome, String>) {
        match result {
            Ok(TabUiStepOutcome::Applied) => {}
            Ok(TabUiStepOutcome::CommittedCleanupPending { error, retry }) => {
                self.split_persist_retry.borrow_mut().push(retry);
                self.hooks
                    .warn("[shell] split memory write is pending:", &error);
            }
            Ok(TabUiStepOutcome::AppliedWithClosePermit(_))
            | Ok(TabUiStepOutcome::Superseded(_))
            | Ok(TabUiStepOutcome::BrowserSuperseded(_)) => self.hooks.warn(
                "[shell] split memory write failed:",
                "The captured operation was superseded",
            ),
            Err(error) => self
                .hooks
                .warn("[shell] split memory write failed:", &error),
        }
    }

    pub fn retry_split_persist(&self) -> bool {
        let retry = self.split_persist_retry.borrow_mut().pop();
        if let Some(retry) = retry {
            let state = self.rc();
            retry(Box::new(move |result| state.finish_split_persist(result)));
            true
        } else {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity)]
    fn queue_activation(
        &self,
        plan: Plan,
        authority: TabActivationAuthority,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
        pending_failure: Option<Rc<dyn Fn(&str)>>,
        settled: Option<TabUiDone>,
    ) {
        let state = self.this.clone();
        let action: Box<dyn FnOnce()> = Box::new(move || match state.upgrade() {
            Some(state) => start(
                state,
                plan,
                authority,
                target,
                participants,
                cancel,
                pending,
                done,
                pending_failure,
                settled,
            ),
            None => {
                let error = "The tab manager is unavailable".to_owned();
                if let Some(settled) = settled {
                    settled(Err(error.clone()));
                }
                done(Err(error));
            }
        });
        if self.activation_running.replace(true) {
            self.activation_queue.borrow_mut().push_back(action);
        } else {
            action();
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity)]
    pub(crate) fn activate_loaded_authorized(
        &self,
        id: &str,
        authority: TabActivationAuthority,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
        pending_failure: Option<Rc<dyn Fn(&str)>>,
        settled: Option<TabUiDone>,
    ) {
        self.queue_activation(
            Plan::Activate(id.to_owned()),
            authority,
            target,
            participants,
            cancel,
            pending,
            done,
            pending_failure,
            settled,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn activate_loaded_owned_many(
        &self,
        id: &str,
        origin: NativeRequestOwner,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        self.queue_activation(
            Plan::Activate(id.to_owned()),
            TabActivationAuthority::Interactive(origin),
            target,
            participants,
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn activate_loaded_native_many(
        &self,
        id: &str,
        document: shell_contract::ipc::NativeInvocationDocument,
        owner: NativeRequestOwner,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        self.queue_activation(
            Plan::Activate(id.to_owned()),
            TabActivationAuthority::NativeInvocation { document, owner },
            target,
            participants,
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn activate_loaded_native_operation_many(
        &self,
        id: &str,
        operation: shell_contract::ipc::NativeInvocationOperation,
        owner: NativeRequestOwner,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        self.queue_activation(
            Plan::Activate(id.to_owned()),
            TabActivationAuthority::NativeOperation { operation, owner },
            target,
            participants,
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn activate_loaded_startup_recovery_many(
        &self,
        id: &str,
        startup: shell_contract::ipc::NativeInvocationOperation,
        shell: NativeRequestOwner,
        child: shell_contract::ipc::NativeInvocationDocument,
        owner: NativeRequestOwner,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        self.queue_activation(
            Plan::Activate(id.to_owned()),
            TabActivationAuthority::StartupRecovery {
                startup,
                shell,
                child,
                owner,
            },
            target,
            participants,
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_loaded_split_startup(
        &self,
        startup: shell_contract::ipc::NativeInvocationOperation,
        shell: NativeRequestOwner,
        a: (String, NativeRequestOwner),
        b: (String, NativeRequestOwner),
        orientation: main_core::shell::split_layout::SplitOrientation,
        ratio: f64,
        focus: main_core::shell::split_layout::Pane,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        let target = match focus {
            main_core::shell::split_layout::Pane::A => a.1.clone(),
            main_core::shell::split_layout::Pane::B => b.1.clone(),
        };
        let plan = Plan::Split {
            a: a.0,
            b: b.0,
            a_view: a.1.view,
            b_view: b.1.view,
            orientation,
            ratio,
            focus,
        };
        self.queue_activation(
            plan,
            TabActivationAuthority::NativeOperation {
                operation: startup,
                owner: shell,
            },
            target,
            vec![a.1, b.1],
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_loaded_split_startup_command(
        &self,
        startup: shell_contract::ipc::NativeInvocationOperation,
        shell: NativeRequestOwner,
        child: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        a: (String, NativeRequestOwner),
        b: (String, NativeRequestOwner),
        orientation: main_core::shell::split_layout::SplitOrientation,
        ratio: f64,
        focus: main_core::shell::split_layout::Pane,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        let target = match focus {
            main_core::shell::split_layout::Pane::A => a.1.clone(),
            main_core::shell::split_layout::Pane::B => b.1.clone(),
        };
        let plan = Plan::Split {
            a: a.0,
            b: b.0,
            a_view: a.1.view,
            b_view: b.1.view,
            orientation,
            ratio,
            focus,
        };
        let authority = TabActivationAuthority::StartupCommand {
            startup,
            shell,
            child,
            expected,
            owner: target.clone(),
        };
        self.queue_activation(
            plan,
            authority,
            target,
            vec![a.1, b.1],
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    pub fn layout_owned_many(
        &self,
        origin: NativeRequestOwner,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        self.queue_activation(
            Plan::Layout,
            TabActivationAuthority::Interactive(origin),
            target,
            participants,
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    pub fn restore_layout_owned_many(
        &self,
        origin: NativeRequestOwner,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        self.queue_activation(
            Plan::Restore,
            TabActivationAuthority::Interactive(origin),
            target,
            participants,
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_content_bleed_owned(
        &self,
        origin: NativeRequestOwner,
        target: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        effects: OwnedEffectsHost,
        on: bool,
        done: TabUiDone,
    ) {
        if self
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .is_none_or(|installed| !Arc::ptr_eq(installed, &effects))
        {
            return done(Err(
                "The captured activation authority is unavailable".into()
            ));
        }
        let view = target.view;
        self.queue_activation(
            Plan::Bleed { view, on },
            TabActivationAuthority::Interactive(origin),
            target,
            Vec::new(),
            cancel,
            pending,
            done,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reveal_loaded_owned_many(
        &self,
        id: &str,
        origin: NativeRequestOwner,
        target: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: TabUiDone,
    ) {
        if !self.is_detached_tab(id) || self.tab_contents(id) != Some(target.view) {
            return done(Err(
                "The captured detached tab no longer matches its document".into(),
            ));
        }
        self.hooks.reveal_captured_many(
            TabActivationAuthority::Interactive(origin),
            target,
            participants,
            cancel,
            pending,
            done,
        );
    }
}

pub(crate) fn publish_startup_changed(
    state: Rc<TabsState>,
    authority: TabActivationAuthority,
    mut participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    done: TabUiDone,
) {
    let prepared = (|| -> Result<PreparedTabActivationPublication, String> {
        let effects = state
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or("Owned activation is unavailable")?;
        let (shell, changed, split, settings_path, revision) = {
            let mut inner = state.inner.borrow_mut();
            let Inner {
                manager,
                detached,
                bridge,
                split_memory_clock,
                ..
            } = &mut *inner;
            let manager = manager.as_ref().ok_or("The shell window is unavailable")?;
            let shell = bridge.shell.ok_or("The shell window is unavailable")?;
            let changed = TabsChanged {
                tabs: manager.list(),
                open_paths: manager.open_file_paths(),
                detached_paths: detached.detached_file_paths(bridge),
            };
            (
                shell,
                changed,
                manager.split_state(),
                bridge.settings_path.clone(),
                split_memory_clock
                    .capture()
                    .ok_or("The split memory revision is unavailable")?,
            )
        };
        let shell_owner = capture_shell_owner(&state, &effects, shell)?;
        let native = state
            .hooks
            .prepare_activation_ui(shell, &shell_owner, None, &[])?;
        let (session_strip, session_detached) = state.session_snapshot()?;
        let mut tab_views = Vec::new();
        for tab in &changed.tabs {
            if let Some(view) = state.tab_contents(&tab.id) {
                tab_views.push((tab.id.clone(), view));
            }
        }
        for row in &session_detached {
            if let Some(id) = row.get("id").and_then(serde_json::Value::as_str)
                && let Some(view) = state.tab_contents(id)
            {
                tab_views.push((id.to_owned(), view));
            }
        }
        for (_, view) in &tab_views {
            if !participants.iter().any(|owner| owner.view == *view)
                && authority.owner().view != *view
                && native.shell.view != *view
            {
                let generation = state
                    .services
                    .views
                    .renderer_generation(*view)
                    .ok_or("A captured tab renderer is unavailable")?;
                participants.push(effects.capture_owner(*view, &generation)?);
            }
        }
        let snapshot = TabActivationPublicationSnapshot {
            changed,
            tab_views,
            split,
            settings_path,
            session_strip,
            session_detached,
            split_memory_revision: revision,
        };
        state.hooks.prepare_activation_publication_owned(
            authority,
            native.shell,
            participants,
            cancel,
            pending,
            effects,
            snapshot,
        )
    })();
    match prepared {
        Ok(publication) => (publication.changed)(Box::new(move |result| {
            finish_startup_publication(state, result, Some(done))
        })),
        Err(error) => done(Err(error)),
    }
}

fn finish_startup_publication(
    state: Rc<TabsState>,
    result: Result<TabUiStepOutcome, String>,
    done: Option<TabUiDone>,
) {
    let result = match result {
        Ok(TabUiStepOutcome::Applied) => Ok(()),
        Ok(TabUiStepOutcome::CommittedCleanupPending { error, retry }) => {
            state.startup_publication_retries.borrow_mut().push(retry);
            Err(error)
        }
        Ok(TabUiStepOutcome::AppliedWithClosePermit(_))
        | Ok(TabUiStepOutcome::Superseded(_))
        | Ok(TabUiStepOutcome::BrowserSuperseded(_)) => {
            Err("The startup publication was superseded".into())
        }
        Err(error) => Err(error),
    };
    if let Some(done) = done {
        done(result);
    } else if let Err(error) = result {
        state
            .hooks
            .warn("[shell] startup publication is pending:", &error);
    }
}

impl TabsState {
    pub fn retry_startup_publication(&self) -> bool {
        let retry = self.startup_publication_retries.borrow_mut().pop();
        if let Some(retry) = retry {
            let state = self.rc();
            retry(Box::new(move |result| {
                finish_startup_publication(state, result, None)
            }));
            true
        } else {
            false
        }
    }
}
