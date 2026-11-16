use super::Docs;
use crate::kit::ViewMap;
use shell_contract::ViewId;
use shell_contract::runtime::{RendererGeneration, Services};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Consumed,
    RolledBack,
    Removed,
    Replaced,
}
struct Entry {
    generation: RendererGeneration,
    identity: Rc<Cell<State>>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        if self.identity.get() == State::Pending {
            self.identity.set(State::Removed);
        }
    }
}
pub(super) struct BlankSeeds(ViewMap<Entry>);
impl BlankSeeds {
    pub fn new(services: &Services) -> Self {
        Self(ViewMap::new(services))
    }
    pub fn get(&self, view: ViewId) -> Option<RendererGeneration> {
        self.0
            .with(view, |entry| entry.map(|entry| entry.generation.clone()))
    }
    pub fn remove(&self, view: ViewId) -> Option<RendererGeneration> {
        self.0.remove(view).map(|entry| {
            entry.identity.set(State::Consumed);
            entry.generation.clone()
        })
    }
}
pub struct DocsPreparedBlank {
    docs: Rc<Docs>,
    view: ViewId,
    generation: RendererGeneration,
    expected: Option<Rc<Cell<State>>>,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: Option<shell_contract::ipc::NativeBrowserOwner>,
    identity: Rc<Cell<State>>,
    entry: Option<Entry>,
    displaced: Option<Entry>,
    committed: bool,
}
pub struct DocsBlankReceipt {
    docs: Rc<Docs>,
    view: ViewId,
    generation: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    identity: Rc<Cell<State>>,
    displaced: RefCell<Option<Entry>>,
    closed: Cell<bool>,
}
pub struct DocsValidatedBlank<'a> {
    prepared: &'a mut DocsPreparedBlank,
    owner: shell_contract::ipc::NativeBrowserOwner,
    _transition: std::marker::PhantomData<&'a shell_contract::ipc::NativeBrowserTransition<'a>>,
}

pub(super) fn prepare(
    docs: &Rc<Docs>,
    view: ViewId,
    generation: RendererGeneration,
    reservation: &shell_contract::ipc::NativeClosureReservation,
) -> Result<DocsPreparedBlank, String> {
    if !reservation.matches(view, &generation) {
        return Err("The close reservation does not match the document view.".into());
    }
    if !docs
        .services
        .views
        .renderer_generation(view)
        .is_some_and(|actual| actual.same_generation(&generation) && actual.is_active())
    {
        return Err("The document view is no longer available.".into());
    }
    docs.owned_new_blank.0.reserve(1);
    let expected = docs
        .owned_new_blank
        .0
        .with(view, |entry| entry.map(|entry| Rc::clone(&entry.identity)));
    let identity = Rc::new(Cell::new(State::Pending));
    Ok(DocsPreparedBlank {
        docs: Rc::clone(docs),
        view,
        generation: generation.clone(),
        reservation: reservation.clone(),
        owner: None,
        expected,
        identity: Rc::clone(&identity),
        entry: Some(Entry {
            generation,
            identity,
        }),
        displaced: None,
        committed: false,
    })
}
impl DocsPreparedBlank {
    pub fn validate_in_transition<'a>(
        &'a mut self,
        transition: &'a shell_contract::ipc::NativeBrowserTransition<'a>,
        owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<DocsValidatedBlank<'a>, String> {
        if !transition.matches(&self.reservation)
            || !(transition.matches_owner(owner) || transition.proposes_owner(owner))
        {
            return Err("The blank seed browser owner changed.".into());
        }
        if self.committed || self.entry.is_none() {
            return Err("The blank seed was already committed.".into());
        }
        let current = self.docs.owned_new_blank.0.with(self.view, |entry| {
            entry.map(|entry| Rc::clone(&entry.identity))
        });
        if !match (&current, &self.expected) {
            (None, None) => true,
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            _ => false,
        } {
            return Err("The blank seed changed.".into());
        }
        Ok(DocsValidatedBlank {
            prepared: self,
            owner: owner.clone(),
            _transition: std::marker::PhantomData,
        })
    }
    pub fn commit_admitted(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<(), String> {
        self.validate_in_transition(transition, owner)?.commit();
        Ok(())
    }
    pub fn finish(mut self) -> Result<DocsBlankReceipt, String> {
        if !self.committed {
            return Err("The blank seed was not committed.".into());
        }
        Ok(DocsBlankReceipt {
            docs: Rc::clone(&self.docs),
            view: self.view,
            generation: self.generation.clone(),
            reservation: self.reservation.clone(),
            owner: self
                .owner
                .take()
                .ok_or("The blank seed browser owner is unavailable.")?,
            identity: Rc::clone(&self.identity),
            displaced: RefCell::new(self.displaced.take()),
            closed: Cell::new(false),
        })
    }
}
impl DocsValidatedBlank<'_> {
    fn apply(prepared: &mut DocsPreparedBlank, owner: shell_contract::ipc::NativeBrowserOwner) {
        prepared.displaced = prepared.docs.owned_new_blank.0.insert(
            prepared.view,
            prepared.entry.take().expect("validated blank seed"),
        );
        if let Some(previous) = &prepared.displaced {
            previous.identity.set(State::Replaced);
        }
        prepared.owner = Some(owner);
        prepared.committed = true;
    }
    pub fn commit(self) {
        Self::apply(self.prepared, self.owner);
    }
    pub fn commit_with_receipt(self) -> DocsBlankReceipt {
        let Self {
            prepared, owner, ..
        } = self;
        Self::apply(prepared, owner.clone());
        DocsBlankReceipt {
            docs: Rc::clone(&prepared.docs),
            view: prepared.view,
            generation: prepared.generation.clone(),
            reservation: prepared.reservation.clone(),
            owner,
            identity: Rc::clone(&prepared.identity),
            displaced: RefCell::new(prepared.displaced.take()),
            closed: Cell::new(false),
        }
    }
}
impl DocsBlankReceipt {
    pub fn retire_closed(
        &self,
        receipt: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !receipt.matches(&self.reservation, self.view, &self.generation) {
            return Err("The close receipt belongs to another document view.".into());
        }
        self.closed.set(true);
        Ok(())
    }
    pub fn retire_adopted(
        &self,
        receipt: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        self.retire_closed(receipt)?;
        if receipt.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed {
            return Err("The adopted document was not closed.".into());
        }
        match self.identity.get() {
            State::Consumed | State::RolledBack | State::Removed => return Ok(()),
            State::Replaced => return Err("The blank seed was replaced.".into()),
            State::Pending => {}
        }
        let same = self.docs.owned_new_blank.0.with(self.view, |entry| {
            entry.is_some_and(|entry| Rc::ptr_eq(&entry.identity, &self.identity))
        });
        if !same {
            return Err("The blank seed is no longer available.".into());
        }
        self.docs.owned_new_blank.remove(self.view);
        Ok(())
    }
    pub fn retire_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, &self.owner) {
            return Err("The blank seed belongs to another browser owner.".into());
        }
        match self.identity.get() {
            State::Consumed | State::Replaced => Ok(()),
            _ => Err("The superseded blank seed is not yet retired.".into()),
        }
    }
    pub fn was_consumed(&self) -> bool {
        self.identity.get() == State::Consumed
    }
    pub fn rollback(&self) -> Result<(), String> {
        match self.identity.get() {
            State::Consumed | State::RolledBack => return Ok(()),
            State::Removed if self.closed.get() => {
                if self
                    .docs
                    .services
                    .views
                    .renderer_generation(self.view)
                    .is_some_and(|actual| !actual.same_generation(&self.generation))
                    || self.docs.owned_new_blank.0.contains(self.view)
                {
                    return Err("The blank seed was replaced.".into());
                }
                if let Some(previous) = self.displaced.borrow_mut().take() {
                    previous.identity.set(State::Pending);
                    self.docs.owned_new_blank.0.insert(self.view, previous);
                }
                self.identity.set(State::RolledBack);
                return Ok(());
            }
            State::Removed => {
                return Err("The blank seed was removed without an exact close receipt.".into());
            }
            State::Replaced => return Err("The blank seed was replaced.".into()),
            State::Pending => {}
        }
        if self
            .docs
            .services
            .views
            .renderer_generation(self.view)
            .is_some_and(|actual| !actual.same_generation(&self.generation))
        {
            return Err("The document renderer was replaced.".into());
        }
        let same = self.docs.owned_new_blank.0.with(self.view, |entry| {
            entry.is_some_and(|entry| Rc::ptr_eq(&entry.identity, &self.identity))
        });
        if !same {
            return Err("The blank seed is no longer available.".into());
        }
        if let Some(previous) = self.displaced.borrow_mut().take() {
            previous.identity.set(State::Pending);
            self.docs.owned_new_blank.0.insert(self.view, previous);
        } else {
            self.docs.owned_new_blank.remove(self.view);
        }
        self.identity.set(State::RolledBack);
        Ok(())
    }
}
