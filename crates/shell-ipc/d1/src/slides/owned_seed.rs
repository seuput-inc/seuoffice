use std::cell::{Cell, RefCell};
use std::rc::Rc;
use main_core::slides::seed_queue::{SeedEntry, SlidesSeedOptions};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, NativeRequestOwner};
use shell_contract::runtime::{CancelToken, RendererGeneration};
use shell_contract::{IpcError, IpcValue, ViewId};
use crate::owned_seed::SeedAuthority;
use crate::value::{obj, thrown};
use super::state::{App, DeckHandle, SlideSession, StagedBinding};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SeedStatus { Prepared, Queued, Consumed, Retired, RolledBack }

pub(super) struct OwnedSeed {
    authority: SeedAuthority,
    entry: SeedEntry,
    status: Rc<Cell<SeedStatus>>,
}

pub struct SlidesSeedRollback {
    state: Rc<RefCell<super::state::AppState>>,
    view: ViewId,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: Option<shell_contract::ipc::NativeBrowserOwner>,
    status: Rc<Cell<SeedStatus>>,
    closed: Cell<bool>,
    displaced: RefCell<Option<OwnedSeed>>,
}

pub struct PreparedSlidesSeed {
    state: Rc<RefCell<super::state::AppState>>,
    view: ViewId,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: shell_contract::ipc::NativeBrowserOwner,
    expected: Option<Rc<Cell<SeedStatus>>>,
    seed: OwnedSeed,
    receipt: Rc<SlidesSeedRollback>,
}

impl PreparedSlidesSeed {
    pub fn receipt(&self) -> Rc<SlidesSeedRollback> { self.receipt.clone() }

    pub fn commit(self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>, initialize: &mut dyn FnMut() -> Result<(), String>) -> Result<Rc<SlidesSeedRollback>, String> {
        if !transition.matches(&self.reservation) || !(transition.matches_owner(&self.browser) || transition.proposes_owner(&self.browser)) {
            return Err("The presentation seed browser changed".into());
        }
        if self.receipt.status.get() != SeedStatus::Prepared {
            return Err("The presentation seed preparation was cancelled".into());
        }
        let mut state = self.state.try_borrow_mut().map_err(|_| "The presentation seed state is busy")?;
        let current = state.owned_seeds.get(&self.view).map(|seed| &seed.status);
        if !match (current, self.expected.as_ref()) {
            (None, None) => true,
            (Some(current), Some(expected)) => Rc::ptr_eq(current, expected),
            _ => false,
        } {
            return Err("The presentation seed changed before admission".into());
        }
        if current.is_none() && state.owned_seeds.len() == state.owned_seeds.capacity() {
            return Err("The presentation seed reservation was consumed".into());
        }
        initialize()?;
        let displaced = state.owned_seeds.insert(self.view, self.seed);
        *self.receipt.displaced.borrow_mut() = displaced;
        self.receipt.status.set(SeedStatus::Queued);
        drop(state);
        Ok(self.receipt)
    }
}

impl SlidesSeedRollback {
    pub fn was_consumed(&self) -> bool {
        self.status.get() == SeedStatus::Consumed
    }
    pub fn retire_closed(&self, proof: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if matches!(self.status.get(), SeedStatus::Prepared | SeedStatus::RolledBack) {
            return Err("The presentation seed was not committed".into());
        }
        if !proof.matches(&self.reservation, self.view, &self.created) {
            return Err("The native close receipt does not match the presentation seed".into());
        }
        if let Some(browser) = &self.browser {
            proof.with_terminal_owner(browser, |transition| {
                if !transition.matches(&self.reservation, browser) { return Err("The presentation terminal owner changed".into()); }
                self.closed.set(true);
                Ok(())
            })
        } else {
            self.closed.set(true);
            Ok(())
        }
    }
    pub fn retire_adopted(&self, proof: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created)
            || proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed {
            return Err("The adopted presentation close receipt does not match".into());
        }
        let retire = || {
            match self.status.get() {
                SeedStatus::Consumed | SeedStatus::Retired => return Ok(None),
                SeedStatus::RolledBack => return Err("The presentation seed was already rolled back".into()),
                SeedStatus::Prepared => return Err("The presentation seed was not committed".into()),
                SeedStatus::Queued => {}
            }
            let mut state = self.state.borrow_mut();
            if !state.owned_seeds.get(&self.view).is_some_and(|seed| Rc::ptr_eq(&seed.status, &self.status)) {
                return Err("The presentation seed was replaced before adopted close".into());
            }
            let removed = state.owned_seeds.remove(&self.view);
            self.status.set(SeedStatus::Retired);
            Ok(removed)
        };
        let removed = if let Some(browser) = &self.browser {
            proof.with_terminal_owner(browser, |transition| {
                if !transition.matches(&self.reservation, browser) { return Err("The presentation terminal owner changed".into()); }
                retire()
            })?
        } else { retire()? };
        drop(removed);
        Ok(())
    }

    pub fn retire_superseded(&self, proof: &shell_contract::ipc::NativeBrowserSuperseded) -> Result<(), String> {
        let browser = self.browser.as_ref().ok_or("The presentation seed has no captured browser owner")?;
        if !proof.matches(&self.reservation, browser) {
            return Err("The supersession belongs to another presentation browser".into());
        }
        match self.status.get() {
            SeedStatus::Prepared | SeedStatus::RolledBack => return Err("The presentation seed was not committed".into()),
            SeedStatus::Retired => return Ok(()),
            SeedStatus::Consumed => {}
            SeedStatus::Queued => {
                let state = self.state.borrow();
                if state.owned_seeds.get(&self.view).is_some_and(|seed| Rc::ptr_eq(&seed.status, &self.status)) {
                    return Err("The superseded presentation seed is still current".into());
                }
            }
        }
        self.status.set(SeedStatus::Retired);
        drop(self.displaced.borrow_mut().take());
        Ok(())
    }



    pub fn rollback(&self) -> Result<(), String> {
        match self.status.get() {
            SeedStatus::Prepared => { self.status.set(SeedStatus::RolledBack); return Ok(()); }
            SeedStatus::Consumed | SeedStatus::Retired | SeedStatus::RolledBack => return Ok(()),
            SeedStatus::Queued => {}
        }
        let mut state = self.state.borrow_mut();
        let matches = state.owned_seeds.get(&self.view)
            .is_some_and(|seed| Rc::ptr_eq(&seed.status, &self.status));
        if !matches { return Err("The presentation seed was replaced before rollback".into()); }
        if !self.created.is_active() && !self.closed.get() { return Err("The presentation seed lifetime ended without a close receipt".into()); }
        let removed = state.owned_seeds.remove(&self.view);
        let mut displaced = self.displaced.borrow_mut().take();
        if displaced.as_ref().is_some_and(|seed| seed.status.get() == SeedStatus::Queued) {
            if let Some(seed) = displaced.take() { state.owned_seeds.insert(self.view, seed); }
        }
        self.status.set(SeedStatus::RolledBack);
        drop(state);
        drop(removed);
        drop(displaced);
        Ok(())
    }
}

pub(super) fn forget_seed_on_view_gone(state: &mut super::state::AppState, view: ViewId) {
    if state.owned_seeds.get(&view).is_some_and(|seed| Rc::strong_count(&seed.status) == 1) {
        state.owned_seeds.remove(&view);
    }
}

pub(super) fn detach_seed_on_view_gone(state: &mut super::state::AppState, view: ViewId) -> Option<OwnedSeed> {
    if !state.owned_seeds.get(&view).is_some_and(|seed| Rc::strong_count(&seed.status) == 1) { return None; }
    let seed = state.owned_seeds.remove(&view)?;
    seed.status.set(SeedStatus::Retired);
    Some(seed)
}

impl super::SlidesViews {
    pub fn prepare_seed_in_transition(&self, view: ViewId, bytes: Vec<u8>, suggested_name: String, source: NativeRequestOwner, cancel: CancelToken, created: RendererGeneration, reservation: shell_contract::ipc::NativeClosureReservation, browser: shell_contract::ipc::NativeBrowserOwner) -> Result<PreparedSlidesSeed, String> {
        if !reservation.matches(view, &created) || !browser.reservation().same_reservation(&reservation) {
            return Err("The presentation seed reservation does not match its target".into());
        }
        let mut state = self.0.state.borrow_mut();
        state.owned_seeds.try_reserve(1).map_err(|_| "The presentation seed could not be reserved")?;
        let expected = state.owned_seeds.get(&view).map(|seed| seed.status.clone());
        drop(state);
        let status = Rc::new(Cell::new(SeedStatus::Prepared));
        let receipt = Rc::new(SlidesSeedRollback { state: self.0.state.clone(), view, created: created.clone(), reservation: reservation.clone(), browser: Some(browser.clone()), status: status.clone(), closed: Cell::new(false), displaced: RefCell::new(None) });
        let seed = OwnedSeed {
            authority: SeedAuthority { source, cancel, created },
            entry: SeedEntry { deck: bytes, options: SlidesSeedOptions { ask_path_on_first_save: Some(true), suggested_name: Some(suggested_name), ..Default::default() } },
            status,
        };
        Ok(PreparedSlidesSeed { state: self.0.state.clone(), view, reservation, browser, expected, seed, receipt })
    }

    pub fn queue_seed_owned(&self, view: ViewId, bytes: Vec<u8>, suggested_name: String, source: NativeRequestOwner, participants: &[NativeRequestOwner], cancel: CancelToken, pending: CancelToken, created: RendererGeneration, reservation: shell_contract::ipc::NativeClosureReservation, initialize: &mut dyn FnMut() -> Result<(), String>) -> Result<SlidesSeedRollback, String> {
        if !reservation.matches(view, &created) { return Err("The presentation seed reservation does not match its target".into()); }
        let effects = self.0.services.hosts.get::<OwnedEffectsHost>().cloned().ok_or_else(|| "The native seed effect host is unavailable".to_owned())?;
        let authority = SeedAuthority { source, cancel, created };
        let status = Rc::new(Cell::new(SeedStatus::Queued));
        let seed = OwnedSeed { authority: authority.clone(), entry: SeedEntry { deck: bytes, options: SlidesSeedOptions { ask_path_on_first_save: Some(true), suggested_name: Some(suggested_name), ..Default::default() } }, status: status.clone() };
        let displaced = authority.stage(&effects, participants, &pending, || {
            initialize()?;
            Ok::<_, String>(self.0.state.borrow_mut().owned_seeds.insert(view, seed))
        })??;
        Ok(SlidesSeedRollback { state: self.0.state.clone(), view, created: authority.created.clone(), reservation, browser: None, status, closed: Cell::new(false), displaced: RefCell::new(displaced) })
    }
}

impl App {
    pub(super) fn open_owned_seed(&self, cx: &CallContext, fit: f64) -> Result<Option<IpcValue>, IpcError> {
        let view = cx.sender.view;
        let captured = self.state.borrow().owned_seeds.get(&view)
            .map(|seed| (seed.authority.clone(), seed.status.clone()));
        let Some((authority, status)) = captured else { return Ok(None) };
        let effects = self.services.hosts.get::<OwnedEffectsHost>().cloned().ok_or_else(|| thrown("The native seed effect host is unavailable"))?;
        let entry = authority.consume(&effects, cx, || {
            let mut state = self.state.borrow_mut();
            if !state.owned_seeds.get(&view).is_some_and(|seed| Rc::ptr_eq(&seed.status, &status)) {
                return None;
            }
            state.owned_seeds.remove(&view)
        })?
            .map(|seed| { seed.status.set(SeedStatus::Consumed); seed.entry })
            .ok_or_else(|| thrown("The seed was already consumed"))?;
        let owner = super::session::render_owner(cx)?;
        if self.state.borrow().has_session(view) { return Err(thrown("The seed destination already contains a presentation")); }
        let deck = self.engine.open(&entry.deck).map_err(|error| thrown(error.0))?;
        let mut session = SlideSession::new("", DeckHandle::new(deck, self.engine.clone()), fit);
        let staged = StagedBinding::new(self.engine.clone(), deck, &owner).map_err(|error| thrown(error.0))?;
        session.ask_path_on_first_save = entry.options.ask_path_on_first_save == Some(true);
        session.first_save_name = entry.options.suggested_name;
        session.core.mark_meta_dirty(); session.core.schedule_history_notify();
        let slides = self.render_all(&session, &owner)?;
        let payload = obj([("slides", slides.clone()), ("size", self.size_value(self.engine.size(deck).map_err(|error| thrown(error.0))?))]);
        let result = self.open_result(&session, slides)?;
        staged.commit(None).map_err(|error| thrown(error.0))?;
        authority.consume(&effects, cx, || {
            let mut state = self.state.borrow_mut();
            if state.has_session(view) { return Err(thrown("The seed destination changed while it was opening")); }
            state.set_initial_session(owner.clone(), session).map_err(thrown)?; Ok(())
        })??;
        effects.emit_owned(&owner, &owner, &cx.cancel, &cx.cancel, "slides:deck-changed", vec![payload]).map_err(thrown)?;
        Ok(Some(authority.consume(&effects, cx, || result)?))
    }
}
