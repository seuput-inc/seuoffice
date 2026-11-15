use super::app::{MindmapApp, Seed};
use crate::owned_seed::SeedAuthority;
use crate::value::thrown;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, NativeRequestOwner};
use shell_contract::runtime::{CancelToken, RendererGeneration};
use shell_contract::{IpcError, ViewId};

pub(super) struct OwnedSeed {
    authority: SeedAuthority,
    seed: Seed,
    consumed: std::rc::Rc<std::cell::Cell<bool>>,
    retired: std::rc::Rc<std::cell::Cell<bool>>,
    pub(super) removed_version: std::rc::Rc<std::cell::RefCell<Option<std::rc::Rc<()>>>>,
}

pub struct MindmapSeedReceipt {
    app: MindmapApp,
    view: ViewId,
    consumed: std::rc::Rc<std::cell::Cell<bool>>,
    retired: std::rc::Rc<std::cell::Cell<bool>>,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: Option<shell_contract::ipc::NativeBrowserOwner>,
    closed: std::cell::Cell<bool>,
    committed: std::cell::Cell<bool>,
    cancelled: std::cell::Cell<bool>,
    removed_version: std::rc::Rc<std::cell::RefCell<Option<std::rc::Rc<()>>>>,
    previous: std::cell::RefCell<Option<Option<OwnedSeed>>>,
}

impl MindmapSeedReceipt {
    pub fn was_consumed(&self) -> bool {
        self.consumed.get()
    }

    pub fn retire_closed(
        &self,
        closure: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !self.committed.get() {
            return Err("The mindmap seed was not committed".into());
        }
        if !closure.matches(&self.reservation, self.view, &self.created) {
            return Err("The close receipt belongs to another mindmap".into());
        }
        self.closed.set(true);
        Ok(())
    }

    pub fn retire_adopted(
        &self,
        closure: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !self.committed.get() {
            return Err("The mindmap seed was not committed".into());
        }
        if !closure.matches(&self.reservation, self.view, &self.created)
            || closure.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed
        {
            return Err("The close receipt belongs to another mindmap".into());
        }
        let mut previous = self.previous.borrow_mut();
        if previous.is_none() {
            return Ok(());
        }
        if self.consumed.get() {
            previous.take();
            return Ok(());
        }
        let mut state = self.app.0.state.borrow_mut();
        let current = state.owned_seeds.get(&self.view);
        let mine =
            current.is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.consumed, &self.consumed));
        let removed = current.is_none()
            && same_version(
                state.owned_seed_versions.get(&self.view),
                self.removed_version.borrow().as_ref(),
            )
            && self.removed_version.borrow().is_some();
        if !mine && !removed {
            return Err("The mindmap seed was replaced before close".into());
        }
        if mine {
            state.owned_seeds.remove(&self.view);
            state
                .owned_seed_versions
                .insert(self.view, std::rc::Rc::new(()));
        }
        previous.take();
        Ok(())
    }

    pub fn retire_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        if !self.committed.get()
            || !self
                .browser
                .as_ref()
                .is_some_and(|browser| proof.matches(&self.reservation, browser))
        {
            return Err("The mindmap seed supersession proof does not match".into());
        }
        let state = self.app.0.state.borrow();
        if state
            .owned_seeds
            .get(&self.view)
            .is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.consumed, &self.consumed))
        {
            return Err("The superseded mindmap seed is still pending".into());
        }
        drop(state);
        self.retired.set(true);
        self.previous.borrow_mut().take();
        Ok(())
    }

    pub fn rollback(&self) -> Result<(), String> {
        if !self.committed.get() {
            self.cancelled.set(true);
            return Ok(());
        }
        let mut previous = self.previous.borrow_mut();
        if previous.is_none() {
            return Ok(());
        }
        if self.consumed.get() {
            previous.take();
            return Ok(());
        }
        let mut state = self.app.0.state.borrow_mut();
        let current = state.owned_seeds.get(&self.view);
        let mine =
            current.is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.consumed, &self.consumed));
        let removed = current.is_none()
            && self.closed.get()
            && same_version(
                state.owned_seed_versions.get(&self.view),
                self.removed_version.borrow().as_ref(),
            )
            && self.removed_version.borrow().is_some();
        if !mine && !removed {
            return Err("The mindmap seed was replaced before rollback".into());
        }
        if mine {
            state.owned_seeds.remove(&self.view);
        }
        if let Some(Some(entry)) = previous.take() {
            if !entry.retired.get() {
                state.owned_seeds.insert(self.view, entry);
            }
        }
        state
            .owned_seed_versions
            .insert(self.view, std::rc::Rc::new(()));
        Ok(())
    }
}

fn same_version(a: Option<&std::rc::Rc<()>>, b: Option<&std::rc::Rc<()>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => std::rc::Rc::ptr_eq(a, b),
        _ => false,
    }
}

pub struct PreparedMindmapSeed {
    app: MindmapApp,
    view: ViewId,
    browser: shell_contract::ipc::NativeBrowserOwner,
    reservation: shell_contract::ipc::NativeClosureReservation,
    version: Option<std::rc::Rc<()>>,
    next_version: std::rc::Rc<()>,
    seed: OwnedSeed,
    receipt: std::rc::Rc<MindmapSeedReceipt>,
}

impl PreparedMindmapSeed {
    pub fn receipt(&self) -> std::rc::Rc<MindmapSeedReceipt> {
        self.receipt.clone()
    }

    pub fn commit(
        self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<std::rc::Rc<MindmapSeedReceipt>, String> {
        if self.receipt.cancelled.get() {
            return Err("The mindmap seed was cancelled before publication".into());
        }
        if !transition.matches(&self.reservation)
            || !(transition.matches_owner(&self.browser)
                || transition.proposes_owner(&self.browser))
        {
            return Err("The mindmap seed browser transition does not match".into());
        }
        let mut state = self.app.0.state.borrow_mut();
        if !same_version(
            state.owned_seed_versions.get(&self.view),
            self.version.as_ref(),
        ) {
            return Err("The mindmap seed changed before publication".into());
        }
        if state.owned_seeds.capacity() == state.owned_seeds.len()
            && !state.owned_seeds.contains_key(&self.view)
            || state.owned_seed_versions.capacity() == state.owned_seed_versions.len()
                && !state.owned_seed_versions.contains_key(&self.view)
        {
            return Err("The mindmap seed capacity was consumed".into());
        }
        initialize()?;
        let previous = state.owned_seeds.insert(self.view, self.seed);
        self.receipt.committed.set(true);
        state
            .owned_seed_versions
            .insert(self.view, self.next_version);
        *self.receipt.previous.borrow_mut() = Some(previous);
        drop(state);
        Ok(self.receipt)
    }
}

impl MindmapApp {
    pub fn prepare_seed_in_transition(
        &self,
        view: ViewId,
        seed: Seed,
        source: NativeRequestOwner,
        cancel: CancelToken,
        created: RendererGeneration,
        reservation: shell_contract::ipc::NativeClosureReservation,
        browser: shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<PreparedMindmapSeed, String> {
        if !reservation.matches(view, &created) {
            return Err("The native closure reservation belongs to another mindmap".into());
        }
        let mut state = self.0.state.borrow_mut();
        state
            .owned_seeds
            .try_reserve(1)
            .map_err(|error| error.to_string())?;
        state
            .owned_seed_versions
            .try_reserve(1)
            .map_err(|error| error.to_string())?;
        let version = state.owned_seed_versions.get(&view).cloned();
        drop(state);
        let consumed = std::rc::Rc::new(std::cell::Cell::new(false));
        let retired = std::rc::Rc::new(std::cell::Cell::new(false));
        let removed_version = std::rc::Rc::new(std::cell::RefCell::new(None));
        let receipt = std::rc::Rc::new(MindmapSeedReceipt {
            app: self.clone(),
            view,
            consumed: consumed.clone(),
            retired: retired.clone(),
            created: created.clone(),
            reservation: reservation.clone(),
            browser: Some(browser.clone()),
            closed: std::cell::Cell::new(false),
            committed: std::cell::Cell::new(false),
            cancelled: std::cell::Cell::new(false),
            removed_version: removed_version.clone(),
            previous: std::cell::RefCell::new(None),
        });
        Ok(PreparedMindmapSeed {
            app: self.clone(),
            view,
            browser,
            reservation,
            version,
            next_version: std::rc::Rc::new(()),
            seed: OwnedSeed {
                authority: SeedAuthority {
                    source,
                    cancel,
                    created,
                },
                seed,
                consumed,
                retired,
                removed_version,
            },
            receipt,
        })
    }

    pub fn queue_seed_owned(
        &self,
        view: ViewId,
        seed: Seed,
        source: NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: CancelToken,
        pending: CancelToken,
        created: RendererGeneration,
        reservation: shell_contract::ipc::NativeClosureReservation,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<std::rc::Rc<MindmapSeedReceipt>, String> {
        if !reservation.matches(view, &created) {
            return Err("The native closure reservation belongs to another mindmap".into());
        }
        let effects = self
            .0
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| "The native seed effect host is unavailable".to_owned())?;
        let authority = SeedAuthority {
            source,
            cancel,
            created,
        };
        let staged = authority.clone();
        let version = self
            .0
            .state
            .borrow()
            .owned_seed_versions
            .get(&view)
            .cloned();
        let consumed = std::rc::Rc::new(std::cell::Cell::new(false));
        let retired = std::rc::Rc::new(std::cell::Cell::new(false));
        let removed_version = std::rc::Rc::new(std::cell::RefCell::new(None));
        let created = authority.created.clone();
        authority.stage(&effects, participants, &pending, || {
            if !same_version(
                self.0.state.borrow().owned_seed_versions.get(&view),
                version.as_ref(),
            ) {
                return Err("The mindmap seed changed before publication".into());
            }
            initialize()?;
            let mut state = self.0.state.borrow_mut();
            if !same_version(state.owned_seed_versions.get(&view), version.as_ref()) {
                return Err("The mindmap seed changed during initialization".into());
            }
            let previous = state.owned_seeds.insert(
                view,
                OwnedSeed {
                    authority: staged,
                    seed,
                    consumed: consumed.clone(),
                    retired: retired.clone(),
                    removed_version: removed_version.clone(),
                },
            );
            state.owned_seed_versions.insert(view, std::rc::Rc::new(()));
            Ok(std::rc::Rc::new(MindmapSeedReceipt {
                app: self.clone(),
                view,
                consumed,
                retired,
                created,
                reservation,
                browser: None,
                closed: std::cell::Cell::new(false),
                committed: std::cell::Cell::new(true),
                cancelled: std::cell::Cell::new(false),
                removed_version,
                previous: std::cell::RefCell::new(Some(previous)),
            }))
        })?
    }

    pub(super) fn take_seed_for(&self, cx: &CallContext) -> Result<Option<Seed>, IpcError> {
        let view = cx.sender.view;
        let authority = self
            .0
            .state
            .borrow()
            .owned_seeds
            .get(&view)
            .map(|entry| entry.authority.clone());
        let Some(authority) = authority else {
            return Ok(self.take_seed(view));
        };
        let effects = self
            .0
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| thrown("The native seed effect host is unavailable"))?;
        authority.consume(&effects, cx, || {
            let mut state = self.0.state.borrow_mut();
            let entry = state.owned_seeds.remove(&view)?;
            entry.consumed.set(true);
            state.owned_seed_versions.insert(view, std::rc::Rc::new(()));
            Some(entry.seed)
        })
    }
}
