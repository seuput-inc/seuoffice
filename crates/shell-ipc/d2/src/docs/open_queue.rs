use std::cell::{Cell, RefCell};
use std::rc::Rc;
use shell_contract::runtime::{RendererGeneration, Services};
use shell_contract::ViewId;
use crate::kit::ViewMap;
use super::Docs;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenState { Pending, Consumed, Removed, Replaced }
struct QueuedOpen { path: String, identity: Rc<Cell<OpenState>> }
impl Drop for QueuedOpen {
    fn drop(&mut self) {
        if self.identity.get() == OpenState::Pending { self.identity.set(OpenState::Removed); }
    }
}

pub(super) struct OpenQueue(ViewMap<QueuedOpen>);
impl OpenQueue {
    pub fn new(services: &Services) -> Self { Self(ViewMap::new(services)) }
    pub fn get(&self, view: ViewId) -> Option<String> { self.0.with(view, |entry| entry.map(|entry| entry.path.clone())) }
    pub fn insert(&self, view: ViewId, path: String) -> Option<String> {
        self.0.insert(view, QueuedOpen { path, identity: Rc::new(Cell::new(OpenState::Pending)) }).map(|mut entry| {
            entry.identity.set(OpenState::Replaced);
            std::mem::take(&mut entry.path)
        })
    }
    pub fn remove(&self, view: ViewId) -> Option<String> {
        self.0.remove(view).map(|mut entry| { entry.identity.set(OpenState::Consumed); std::mem::take(&mut entry.path) })
    }
}

pub struct DocsPreparedOpen {
    docs: Rc<Docs>, view: ViewId, created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: Option<shell_contract::ipc::NativeBrowserOwner>,
    expected: Option<Rc<Cell<OpenState>>>, entry: Option<QueuedOpen>, displaced: Option<QueuedOpen>,
    identity: Rc<Cell<OpenState>>, committed: bool,
}
pub struct DocsOpenReceipt {
    docs: Rc<Docs>, view: ViewId, created: RendererGeneration, identity: Rc<Cell<OpenState>>,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    displaced: RefCell<Option<QueuedOpen>>,
    closed: Cell<bool>,
}
pub struct DocsValidatedOpen<'a> {
    prepared: &'a mut DocsPreparedOpen,
    owner: shell_contract::ipc::NativeBrowserOwner,
    _transition: std::marker::PhantomData<&'a shell_contract::ipc::NativeBrowserTransition<'a>>,
}

pub(super) fn prepare(docs: &Rc<Docs>, view: ViewId, path: String, created: RendererGeneration,
    reservation: &shell_contract::ipc::NativeClosureReservation) -> Result<DocsPreparedOpen, String> {
    if !reservation.matches(view, &created) { return Err("The close reservation does not match the document view.".into()); }
    if !docs.services.views.renderer_generation(view).is_some_and(|actual| actual.same_generation(&created) && actual.is_active()) {
        return Err("The document view is no longer available.".into());
    }
    docs.window_opens.0.reserve(1);
    let expected = docs.window_opens.0.with(view, |entry| entry.map(|entry| Rc::clone(&entry.identity)));
    let identity = Rc::new(Cell::new(OpenState::Pending));
    Ok(DocsPreparedOpen { docs: Rc::clone(docs), view, created, reservation: reservation.clone(), owner: None, expected, entry: Some(QueuedOpen { path, identity: Rc::clone(&identity) }), displaced: None, identity, committed: false })
}
impl DocsPreparedOpen {
    pub fn validate_in_transition<'a>(&'a mut self, transition: &'a shell_contract::ipc::NativeBrowserTransition<'a>, owner: &shell_contract::ipc::NativeBrowserOwner) -> Result<DocsValidatedOpen<'a>, String> {
        if !transition.matches(&self.reservation) || !(transition.matches_owner(owner) || transition.proposes_owner(owner)) { return Err("The document open browser owner changed.".into()); }
        if self.committed || self.entry.is_none() { return Err("The document open was already queued.".into()); }
        let current = self.docs.window_opens.0.with(self.view, |entry| entry.map(|entry| Rc::clone(&entry.identity)));
        let same = match (&current, &self.expected) {
            (None, None) => true, (Some(a), Some(b)) => Rc::ptr_eq(a, b), _ => false,
        };
        if !same { return Err("The pending document open changed.".into()); }
        Ok(DocsValidatedOpen { prepared: self, owner: owner.clone(), _transition: std::marker::PhantomData })
    }
    pub fn commit_admitted(&mut self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>, owner: &shell_contract::ipc::NativeBrowserOwner) -> Result<(), String> {
        self.validate_in_transition(transition, owner)?.commit();
        Ok(())
    }
    pub fn finish(mut self) -> Result<DocsOpenReceipt, String> {
        if !self.committed { return Err("The document open was not queued.".into()); }
        Ok(DocsOpenReceipt { docs: Rc::clone(&self.docs), view: self.view, created: self.created.clone(), reservation: self.reservation.clone(), owner: self.owner.take().ok_or("The document open browser owner is unavailable.")?,
            identity: Rc::clone(&self.identity), displaced: RefCell::new(self.displaced.take()), closed: Cell::new(false) })
    }
}
impl DocsValidatedOpen<'_> {
    pub fn commit(self) {
        let prepared = self.prepared;
        prepared.displaced = prepared.docs.window_opens.0.insert(prepared.view, prepared.entry.take().expect("validated document open"));
        if let Some(previous) = &prepared.displaced { previous.identity.set(OpenState::Replaced); }
        prepared.owner = Some(self.owner);
        prepared.committed = true;
    }
}
impl DocsOpenReceipt {
    pub fn retire_closed(&self, receipt: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if !receipt.matches(&self.reservation, self.view, &self.created) { return Err("The close receipt belongs to another document view.".into()); }
        self.closed.set(true);
        Ok(())
    }

    pub fn retire_adopted(&self, receipt: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        self.retire_closed(receipt)?;
        if receipt.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed { return Err("The adopted document was not closed.".into()); }
        match self.identity.get() {
            OpenState::Consumed | OpenState::Removed => return Ok(()),
            OpenState::Replaced => return Err("The pending document open was replaced.".into()),
            OpenState::Pending => {}
        }
        let same = self.docs.window_opens.0.with(self.view, |entry| entry.is_some_and(|entry| Rc::ptr_eq(&entry.identity, &self.identity)));
        if !same { return Err("The pending document open is no longer available.".into()); }
        self.docs.window_opens.remove(self.view);
        Ok(())
    }
    pub fn retire_superseded(&self, proof: &shell_contract::ipc::NativeBrowserSuperseded) -> Result<(), String> {
        if !proof.matches(&self.reservation, &self.owner) { return Err("The pending open belongs to another browser owner.".into()); }
        match self.identity.get() {
            OpenState::Consumed | OpenState::Replaced => Ok(()),
            _ => Err("The superseded pending open is not yet retired.".into()),
        }
    }
    pub fn discard(&self) -> Result<(), String> {
        if self.identity.get() == OpenState::Consumed { return Ok(()); }
        if self.identity.get() == OpenState::Removed {
            if !self.closed.get() { return Err("The pending document open was removed without an exact close receipt.".into()); }
            if self.docs.services.views.renderer_generation(self.view).is_some_and(|actual| !actual.same_generation(&self.created)) {
                return Err("The document renderer was replaced.".into());
            }
            if self.docs.window_opens.0.contains(self.view) { return Err("The pending document open was replaced.".into()); }
            if let Some(previous) = self.displaced.borrow_mut().take() {
                previous.identity.set(OpenState::Pending);
                self.docs.window_opens.0.insert(self.view, previous);
            }
            self.identity.set(OpenState::Consumed);
            return Ok(());
        }
        if self.identity.get() == OpenState::Replaced { return Err("The pending document open was replaced.".into()); }
        if self.docs.services.views.renderer_generation(self.view).is_some_and(|actual| !actual.same_generation(&self.created)) {
            return Err("The document renderer was replaced.".into());
        }
        let matches = self.docs.window_opens.0.with(self.view, |entry| entry.is_some_and(|entry| Rc::ptr_eq(&entry.identity, &self.identity)));
        if !matches { return Err("The pending document open is no longer available.".into()); }
        if let Some(previous) = self.displaced.borrow_mut().take() {
            previous.identity.set(OpenState::Pending);
            self.docs.window_opens.0.insert(self.view, previous);
            self.identity.set(OpenState::Consumed);
        } else {
            self.docs.window_opens.remove(self.view);
        }
        Ok(())
    }
}
