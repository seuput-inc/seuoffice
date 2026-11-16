//! Loading a tab the strip restored unloaded, under a captured authority.
//!
//! Every path that needs such a tab's editor view (a click, a menu, a split
//! drop, a tear-off, a service lookup) comes here: an activating load
//! ([`TabsState::activate_tab_native`], `reveal_tab_owned`) or a load that
//! leaves the tab where it is ([`TabsState::hydrate_tab_owned`],
//! [`TabsState::hydrate_tab_native`]). Both run the tab's opener through the
//! owned recovery adoption, so the closure reservation and the authority gates
//! are the ones of every other restored tab.
use std::cell::RefCell;
use std::rc::Rc;

use shell_contract::ipc::{NativeEditorCommand, NativeInvocationOperation, NativeRequestOwner};
use shell_contract::runtime::CancelToken;

use super::{
    Completion, Done, NativeOpened, PendingOwnedOpen, PreparedSurvivorHydration, Step,
    SurvivorHydration, TabsState,
};
use crate::tabs::host::ShellWindow;

pub type LoadDone = Box<dyn FnOnce(Result<(), String>)>;

impl TabsState {
    /// A native-initiated (menu, OS, service) activation of one tab: it loads
    /// first when it is unloaded, then becomes the active tab. `done` runs once
    /// the activation settled.
    pub fn activate_tab_native(&self, id: &str, done: LoadDone) {
        let cancel = CancelToken::default();
        let expected = NativeEditorCommand::ActivateTab {
            tab_id: id.to_owned(),
        };
        let operation = match self
            .hooks
            .issue_native_command(expected.clone(), cancel.clone())
        {
            Ok(operation) => operation,
            Err(error) => return done(Err(error)),
        };
        self.activate_native_existing(
            id.to_owned(),
            operation,
            expected,
            None,
            Box::new(move |result| {
                drop(cancel);
                done(result.map(|_| ()));
            }),
        );
    }

    /// The load a manager step asked for with `request_tab_load`: it runs on
    /// the core thread after that step returned.
    pub(crate) fn load_requested(&self, id: &str) {
        let hooks = self.hooks.clone();
        self.activate_tab_native(
            id,
            Box::new(move |result| {
                if let Err(error) = result {
                    hooks.warn("[tabs] a tab could not be activated:", &error);
                }
            }),
        );
    }

    /// An application service needs the tab's editor view: an unloaded tab
    /// loads under a native command and keeps its place in the strip.
    pub fn hydrate_tab_native(&self, id: &str, done: LoadDone) {
        let cancel = CancelToken::default();
        let expected = NativeEditorCommand::HydrateTab {
            tab_id: id.to_owned(),
        };
        let operation = match self
            .hooks
            .issue_native_command(expected.clone(), cancel.clone())
        {
            Ok(operation) => operation,
            Err(error) => return done(Err(error)),
        };
        self.hydrate_native_existing(
            id.to_owned(),
            operation,
            expected,
            Box::new(move |result| {
                drop(cancel);
                done(result);
            }),
        );
    }

    /// A renderer request needs the tab's editor view: an unloaded tab loads
    /// under that request's owner and keeps its place in the strip.
    pub fn hydrate_tab_owned(
        &self,
        id: &str,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        done: LoadDone,
    ) {
        let unloaded = self
            .inner
            .borrow()
            .manager
            .as_ref()
            .and_then(|manager| manager.unloaded_tab(id));
        let Some((opener, context)) = unloaded else {
            return done(Ok(()));
        };
        let shell_owner = match self.captured_shell_owner() {
            Ok((_, owner)) => owner,
            Err(error) => return done(Err(error)),
        };
        let state = self.rc();
        self.hydrate_generated_survivor(
            opener,
            context,
            owner,
            shell_owner,
            cancel.clone(),
            cancel,
            Box::new(move |result| done(result.and_then(|outcome| state.settle_survivor(outcome)))),
        );
    }

    fn settle_survivor(&self, outcome: SurvivorHydration) -> Result<(), String> {
        match outcome {
            SurvivorHydration::Ready { .. } => Ok(()),
            SurvivorHydration::AdoptedPending(pending) => {
                let pending = *pending;
                let error = pending.error.clone();
                if let Some(failure) = pending.lifetime {
                    self.retain_failed_adopted_lifetime(
                        failure,
                        pending.generation,
                        pending.closure.reservation.clone(),
                        pending.closure.owner.clone(),
                    );
                }
                Err(error)
            }
        }
    }

    pub(in crate::tabs) fn captured_shell_owner(&self) -> Result<(ShellWindow, NativeRequestOwner), String> {
        let effects = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("Native command effects are unavailable")?;
        let shell = self
            .shell_window()
            .ok_or("The shell window is unavailable")?;
        let generation = self
            .services
            .views
            .renderer_generation(shell.view)
            .ok_or("The shell renderer is unavailable")?;
        let owner = effects
            .capture_owner(shell.view, &generation)
            .map_err(|_| "The shell owner is unavailable".to_owned())?;
        Ok((shell, owner))
    }

    /// Loads each of these tabs that is still unloaded, one after the other and
    /// without activating them, then runs `then` (with the first failure
    /// instead when one fails). A renderer request passes its owner and
    /// cancellation; a menu or service passes `None` and loads under a native
    /// command.
    pub(crate) fn hydrate_tabs(
        &self,
        mut ids: Vec<String>,
        principal: Option<(NativeRequestOwner, CancelToken)>,
        then: LoadDone,
    ) {
        let Some(id) = ids.pop() else {
            return then(Ok(()));
        };
        let state = self.rc();
        let next_principal = principal.clone();
        let next: LoadDone = Box::new(move |result| match result {
            Ok(()) => state.hydrate_tabs(ids, next_principal, then),
            Err(error) => then(Err(error)),
        });
        match principal {
            Some((owner, cancel)) => self.hydrate_tab_owned(&id, owner, cancel, next),
            None => self.hydrate_tab_native(&id, next),
        }
    }

    fn cancel_load_plan(&self, plan: main_core::shell::tab_manager::PreparedTabLoad) {
        self.inner
            .borrow_mut()
            .manager_call(|manager, _| manager.cancel_tab_load(plan));
    }

    fn hydrate_native_existing(
        &self,
        id: String,
        operation: NativeInvocationOperation,
        expected: NativeEditorCommand,
        done: LoadDone,
    ) {
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err("Native command effects are unavailable".into()));
        };
        let cancel = operation.cancel().clone();
        let (shell_window, shell_owner) = match self.captured_shell_owner() {
            Ok(captured) => captured,
            Err(error) => return done(Err(error)),
        };
        let mut prepared = None;
        let admitted = effects.with_native_command_preparation_effect(
            &operation,
            &expected,
            std::slice::from_ref(&shell_owner),
            &cancel,
            &cancel,
            &mut || {
                let mut inner = self.inner.borrow_mut();
                let manager = inner
                    .manager
                    .as_mut()
                    .ok_or("The shell window is unavailable")?;
                prepared = Some(manager.prepare_tab_load(&id)?);
                Ok(())
            },
        );
        if let Err(error) = admitted {
            return done(Err(error));
        }
        let plan = match prepared {
            Some(Some(plan)) => plan,
            Some(None) => return done(Ok(())),
            None => return done(Err("The native tab load was not admitted".into())),
        };
        let operation_id = match self.owned_id() {
            Ok(value) => value,
            Err(error) => {
                self.cancel_load_plan(plan);
                return done(Err(error));
            }
        };
        let Some(generation) = shell_owner.context.renderer().cloned() else {
            self.cancel_load_plan(plan);
            return done(Err("The shell owner has no renderer".into()));
        };
        if let Err(error) = self.begin_creation(operation_id) {
            self.cancel_load_plan(plan);
            return done(Err(error));
        }
        let opener = *plan.opener();
        let context = plan.context().clone();
        let slot = Rc::new(RefCell::new(None));
        let answer = slot.clone();
        let completed = self.rc();
        let finish: Done = Box::new(move |result| {
            completed.finish_creation(operation_id);
            let outcome = answer.borrow_mut().take();
            done(match outcome {
                Some(outcome) => completed.settle_survivor(outcome),
                None => result.and_then(|_| Err("The hydrated tab owner is unavailable".into())),
            });
        });
        self.pending_owned.borrow_mut().insert(
            operation_id,
            PendingOwnedOpen {
                epoch: self.manager_epoch.get(),
                cancel: cancel.clone(),
                generation,
                step: Step::NativeHydrate {
                    plan,
                    command: operation.clone(),
                    shell: shell_owner.clone(),
                    prepared: Some(PreparedSurvivorHydration {
                        outcome: slot,
                        pending: cancel.clone(),
                        shell_owner,
                        shell_window,
                    }),
                },
                done: Completion::Plain(finish),
                generated_cleanup: None,
                diagram: None,
            },
        );
        let state = self.rc();
        self.hooks.open_opener_native_command(
            &opener,
            &context,
            operation,
            cancel.clone(),
            cancel,
            Box::new(move |result| {
                let pending = state.pending_owned.borrow().contains_key(&operation_id);
                if pending {
                    state.finish_owned_open(operation_id, result.map(NativeOpened::Hydrated));
                } else if let Ok(loaded) = result {
                    state.discard_hydration(
                        loaded.attempt,
                        loaded.discard,
                        "The native tab load was cancelled".into(),
                        Box::new(|result| {
                            if let Err(error) = result {
                                eprintln!("[recovery] {error}");
                            }
                        }),
                        None,
                    );
                }
            }),
        );
    }
}
