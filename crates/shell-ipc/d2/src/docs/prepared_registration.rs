use std::cell::Cell;
use std::rc::Rc;

use main_core::docs::byte_handoff::{MediaPermit, MediaViewLease};
use shell_contract::runtime::RendererGeneration;
use shell_contract::{Surface, ViewId};

use super::Docs;

/// Core-thread state prepared before entering the native creation admission.
pub struct DocsPreparedRegistration {
    docs: Rc<Docs>,
    view: ViewId,
    reservation: shell_contract::ipc::NativeClosureReservation,
    created: RendererGeneration,
    lease: MediaViewLease,
    initial_permit: MediaPermit,
    previous: Option<super::DocsMediaSlot>,
    owner: shell_contract::ipc::NativeBrowserOwner,
    cleanup: Option<std::rc::Weak<std::cell::RefCell<Option<super::disposal::DocsActorDisposal>>>>,
    retirement: Rc<super::disposal::ActorRetirement>,
    saved_receipt: Option<Rc<DocsSavedReceipt>>,
    committed_owner: Option<shell_contract::ipc::NativeBrowserOwner>,
    expected_path: Option<String>,
    path: Option<String>,
    registered_path: Option<String>,
    displaced_path: Option<String>,
    was_torn_down: bool,
    #[cfg(any(test, feature = "probe"))]
    synthetic: Option<shell_contract::hosts::synthetic::PreparedSyntheticDocsPublication>,
    #[cfg(any(test, feature = "probe"))]
    synthetic_loss: Option<main_core::docs::byte_handoff::MediaLossSubscription>,
    #[cfg(any(test, feature = "probe"))]
    synthetic_prior: Option<shell_contract::hosts::synthetic::SyntheticDocsPrior>,
    #[cfg(any(test, feature = "probe"))]
    synthetic_previous: Option<super::SyntheticMediaBinding>,
    committed: bool,
}

pub struct DocsRegisteredView {
    docs: Rc<Docs>,
    view: ViewId,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    lease: MediaViewLease,
    initial_permit: MediaPermit,
    registered_path: Option<String>,
    closed: Cell<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SavedState {
    Prepared,
    Committed,
    InFlight,
    Retired,
    Failed,
    Aborted,
}

pub struct DocsSavedReceipt {
    original: DocsRegisteredView,
    candidate: super::disposal::CleanupCandidate,
    state: Rc<Cell<SavedState>>,
    failure: Rc<std::cell::RefCell<Option<String>>>,
    retirement: Rc<super::disposal::ActorRetirement>,
}
impl DocsSavedReceipt {
    fn abort_prepared(&self) -> Result<(), String> {
        match self.state.get() {
            SavedState::Prepared => {
                self.state.set(SavedState::Aborted);
                self.original.lease.revoke();
                self.original
                    .docs
                    .engine
                    .forget_lazy_media_owner(self.original.initial_permit.clone())
            }
            SavedState::Aborted => Ok(()),
            _ => Err("The saved Word actor was already published.".into()),
        }
    }
}

impl shell_contract::editors::SavedActorRegistration for DocsSavedReceipt {
    fn retire_closed(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        then: shell_contract::editors::Then<()>,
    ) {
        if !receipt.matches(
            &self.original.reservation,
            self.original.view,
            &self.original.created,
        ) {
            return then(Err(crate::kit::error(
                "The closed browser belongs to another Word actor",
            )));
        }
        if let Err(error) = receipt.with_terminal_owner(&self.original.owner, |_| Ok(())) {
            return then(Err(crate::kit::error(error)));
        }
        if self.state.get() == SavedState::Failed {
            let error = self
                .failure
                .borrow()
                .clone()
                .unwrap_or_else(|| "The saved Word cleanup failed".into());
            return then(Err(crate::kit::error(error)));
        }
        if !matches!(
            self.state.get(),
            SavedState::Committed | SavedState::InFlight | SavedState::Retired | SavedState::Failed
        ) {
            return then(Err(crate::kit::error(
                "The saved Word actor is not available for retirement",
            )));
        }
        let state = self.state.clone();
        let failure = self.failure.clone();
        let waiter: Box<dyn FnOnce(Result<(), String>)> = Box::new(move |result| {
            match &result {
                Ok(()) => state.set(SavedState::Retired),
                Err(error) if state.get() != SavedState::Committed => {
                    *failure.borrow_mut() = Some(error.clone());
                    state.set(SavedState::Failed);
                }
                Err(_) => {}
            }
            then(result.map_err(crate::kit::error));
        });
        let waiter = match self.retirement.join(waiter) {
            Ok(Some(waiter)) => waiter,
            Ok(None) => return,
            Err((error, waiter)) => return waiter(Err(error)),
        };
        let actor = match self.candidate.try_borrow_mut() {
            Ok(mut candidate) => candidate.take(),
            Err(_) => return waiter(Err("The saved Word cleanup is busy".into())),
        };
        let Some(actor) = actor else {
            return waiter(Err("The saved Word actor was already claimed".into()));
        };
        self.state.set(SavedState::InFlight);
        let state = self.state.clone();
        let candidate = self.candidate.clone();
        actor.run_closed(
            receipt,
            Box::new(move |outcome| match outcome {
                super::disposal::DocsDisposalOutcome::NotRetired { actor, error } => {
                    *candidate.borrow_mut() = Some(*actor);
                    state.set(SavedState::Committed);
                    waiter(Err(error));
                }
                super::disposal::DocsDisposalOutcome::Retired { cleanup } => waiter(cleanup),
            }),
        );
    }

    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        then: shell_contract::editors::Then<()>,
    ) {
        if self.state.get() == SavedState::Failed {
            let error = self
                .failure
                .borrow()
                .clone()
                .unwrap_or_else(|| "The saved Word cleanup failed".into());
            return then(Err(crate::kit::error(error)));
        }
        if self.state.get() != SavedState::Committed {
            return then(Err(crate::kit::error(
                "The saved Word actor is not available for supersession",
            )));
        }
        if let Err(error) = self.original.validate_superseded(&proof) {
            return then(Err(crate::kit::error(error)));
        }
        let original_barrier = match self.original.docs.saved_loads.try_borrow() {
            Ok(loads) => loads
                .get(&self.original.view)
                .is_none_or(|slot| slot.target.browser().same_owner(&self.original.owner)),
            Err(_) => return then(Err(crate::kit::error("The saved Word barrier is busy"))),
        };
        let saved = if original_barrier {
            match super::saved_load::prepare_disposal(
                &self.original.docs,
                self.original.view,
                &self.original.owner,
            ) {
                Ok(saved) => saved.commit(),
                Err(error) => return then(Err(crate::kit::error(error))),
            }
        } else {
            None
        };
        let result = self.original.retire_superseded(&proof);
        if let Some(saved) = saved {
            saved.fail("the saved Word actor was superseded");
        }
        match result {
            Ok(()) => {
                let actor = self.candidate.borrow_mut().take();
                drop(actor);
                self.state.set(SavedState::Retired);
                then(Ok(()));
            }
            Err(error) => {
                *self.failure.borrow_mut() = Some(error.clone());
                self.state.set(SavedState::Failed);
                then(Err(crate::kit::error(error)));
            }
        }
    }

    fn rollback(&self, then: shell_contract::editors::Then<()>) {
        then(self.abort_prepared().map_err(crate::kit::error));
    }
}

impl Drop for DocsPreparedRegistration {
    fn drop(&mut self) {
        if !self.committed
            && let Some(receipt) = &self.saved_receipt
        {
            let _ = receipt.abort_prepared();
        }
    }
}

pub struct DocsValidatedRegistration<'a> {
    prepared: &'a mut DocsPreparedRegistration,
    owner: shell_contract::ipc::NativeBrowserOwner,
    _transition: std::marker::PhantomData<&'a shell_contract::ipc::NativeBrowserTransition<'a>>,
}

pub(super) fn prepare(
    docs: &Rc<Docs>,
    view: ViewId,
    created: RendererGeneration,
    path: Option<String>,
    reservation: &shell_contract::ipc::NativeClosureReservation,
    owner: &shell_contract::ipc::NativeBrowserOwner,
) -> Result<DocsPreparedRegistration, String> {
    if !reservation.matches(view, &created) {
        return Err("The close reservation does not match the document view.".into());
    }
    if !docs
        .services
        .views
        .sender_of(view)
        .is_some_and(|sender| sender.surface == Surface::Docs)
        || !docs
            .services
            .views
            .renderer_generation(view)
            .is_some_and(|actual| actual.same_generation(&created) && actual.is_active())
    {
        return Err("The document view is no longer available.".into());
    }
    prepare_inner(docs, view, created, path, reservation, owner)
}

pub(super) fn prepare_saved(
    docs: &Rc<Docs>,
    target: &shell_contract::ipc::NativeSavedTarget,
) -> Result<DocsPreparedRegistration, String> {
    if target.surface() != Surface::Docs
        || !target.created().is_active()
        || !target
            .reservation()
            .matches(target.view(), target.created())
        || !target
            .browser()
            .reservation()
            .same_reservation(target.reservation())
    {
        return Err("The saved Word reservation is unavailable.".into());
    }
    if docs.media_views.borrow().contains_key(&target.view()) {
        return Err("The saved Word reservation already has an actor.".into());
    }
    let mut prepared = prepare_inner(
        docs,
        target.view(),
        target.created().clone(),
        None,
        target.reservation(),
        target.browser(),
    )?;
    prepared.prepare_fresh_receipt()?;
    Ok(prepared)
}

fn prepare_inner(
    docs: &Rc<Docs>,
    view: ViewId,
    created: RendererGeneration,
    path: Option<String>,
    reservation: &shell_contract::ipc::NativeClosureReservation,
    owner: &shell_contract::ipc::NativeBrowserOwner,
) -> Result<DocsPreparedRegistration, String> {
    let lease = MediaViewLease::new(view.0)?;
    let initial_permit = lease
        .current()
        .ok_or("The document media view is no longer available.")?;
    let previous = docs.media_views.borrow().get(&view).cloned();
    let expected_path = docs.window_opens.get(view);
    let was_torn_down = docs.torn_down.borrow().contains(&view);
    docs.media_views.borrow_mut().reserve(1);
    let registered_path = path.clone();
    Ok(DocsPreparedRegistration {
        docs: Rc::clone(docs),
        view,
        created,
        reservation: reservation.clone(),
        lease,
        initial_permit,
        previous,
        owner: owner.clone(),
        cleanup: None,
        retirement: super::disposal::ActorRetirement::new(),
        saved_receipt: None,
        committed_owner: None,
        expected_path,
        path,
        registered_path,
        displaced_path: None,
        was_torn_down,
        committed: false,
        #[cfg(any(test, feature = "probe"))]
        synthetic: None,
        #[cfg(any(test, feature = "probe"))]
        synthetic_loss: None,
        #[cfg(any(test, feature = "probe"))]
        synthetic_prior: None,
        #[cfg(any(test, feature = "probe"))]
        synthetic_previous: None,
    })
}

impl DocsPreparedRegistration {
    /// Prepares the exact fresh actor and its cleanup receipt before manager admission.
    pub fn prepare_fresh_receipt(
        &mut self,
    ) -> Result<Rc<dyn shell_contract::editors::SavedActorRegistration>, String> {
        if self.committed
            || self.previous.is_some()
            || self.saved_receipt.is_some()
            || self.cleanup.is_some()
        {
            return Err("The fresh Word actor cannot be prepared for publication.".into());
        }
        #[cfg(any(test, feature = "probe"))]
        if let Some(model) = self
            .docs
            .services
            .hosts
            .get::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
        {
            let document = RendererGeneration::new();
            let revoked = document.clone();
            let loss = self.initial_permit.on_revoke(move || revoked.revoke());
            let publication = model.prepare_docs_document(
                self.view,
                &self.created,
                self.initial_permit.owner().generation,
                document,
            )?;
            self.docs
                .synthetic_media
                .borrow_mut()
                .try_reserve(1)
                .map_err(|error| error.to_string())?;
            self.synthetic = Some(publication);
            self.synthetic_loss = Some(loss);
        }
        let candidate = self.prepare_headless_cleanup()?;
        let receipt = Rc::new(DocsSavedReceipt {
            original: DocsRegisteredView {
                docs: self.docs.clone(),
                view: self.view,
                created: self.created.clone(),
                reservation: self.reservation.clone(),
                owner: self.owner.clone(),
                lease: self.lease.clone(),
                initial_permit: self.initial_permit.clone(),
                registered_path: self.registered_path.clone(),
                closed: Cell::new(false),
            },
            candidate,
            state: Rc::new(Cell::new(SavedState::Prepared)),
            failure: Rc::new(std::cell::RefCell::new(None)),
            retirement: self.retirement.clone(),
        });
        self.saved_receipt = Some(receipt.clone());
        Ok(receipt)
    }

    pub fn lease(&self) -> MediaViewLease {
        self.lease.clone()
    }
    pub fn with_saved_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), shell_contract::IpcError>,
    ) -> Result<(), shell_contract::IpcError> {
        #[cfg(any(test, feature = "probe"))]
        if self.synthetic.is_some() {
            if self.synthetic_loss.is_none() {
                return Err(crate::kit::error(
                    "The synthetic Word permit subscription is unavailable",
                ));
            }
            let reservation = self.reservation.clone();
            let mut staged = self.synthetic.take().ok_or_else(|| {
                crate::kit::error("The synthetic Word publication is unavailable")
            })?;
            let result = (|| {
                let publication = staged
                    .validate(transition, &reservation)
                    .map_err(crate::kit::error)?;
                let actor = self
                    .validate_in_transition(transition, None)
                    .map_err(crate::kit::error)?;
                next()?;
                let (binding, prior) = publication.commit();
                actor.commit();
                Ok::<_, shell_contract::IpcError>((binding, prior))
            })();
            match result {
                Ok((binding, prior)) => {
                    let previous = self.docs.synthetic_media.borrow_mut().insert(
                        self.view,
                        super::SyntheticMediaBinding {
                            _publication: Some(binding),
                            _loss: self.synthetic_loss.take(),
                            _successor: None,
                        },
                    );
                    self.synthetic_previous = previous;
                    self.synthetic_prior = Some(prior);
                    return Ok(());
                }
                Err(error) => {
                    self.synthetic = Some(staged);
                    return Err(error);
                }
            }
        }
        let actor = self
            .validate_in_transition(transition, None)
            .map_err(crate::kit::error)?;
        next()?;
        actor.commit();
        Ok(())
    }

    pub fn saved_receipt(
        &self,
    ) -> Result<Rc<dyn shell_contract::editors::SavedActorRegistration>, String> {
        self.saved_receipt
            .as_ref()
            .map(|receipt| {
                receipt.clone() as Rc<dyn shell_contract::editors::SavedActorRegistration>
            })
            .ok_or_else(|| "The saved Word registration was not prepared.".into())
    }

    pub fn prepare_headless_cleanup(
        &mut self,
    ) -> Result<std::rc::Rc<std::cell::RefCell<Option<super::disposal::DocsActorDisposal>>>, String>
    {
        if self.committed || self.previous.is_some() || self.cleanup.is_some() {
            return Err(
                "The Word cleanup candidate cannot be prepared for this registration.".into(),
            );
        }
        let candidate = std::rc::Rc::new(std::cell::RefCell::new(Some(
            super::disposal::DocsActorDisposal::initial(
                self.docs.clone(),
                self.view,
                self.lease.clone(),
                self.initial_permit.clone(),
                self.owner.clone(),
                self.retirement.clone(),
            ),
        )));
        self.cleanup = Some(std::rc::Rc::downgrade(&candidate));
        Ok(candidate)
    }

    /// Called only within the native admission. The token must remain alive until the gate is released.
    pub fn validate_in_transition<'a>(
        &'a mut self,
        transition: &'a shell_contract::ipc::NativeBrowserTransition<'a>,
        successor: Option<&shell_contract::ipc::NativeBrowserOwner>,
    ) -> Result<DocsValidatedRegistration<'a>, String> {
        if !transition.matches(&self.reservation) {
            return Err("The document registration has a different browser reservation.".into());
        }
        if self.committed {
            return Err("The document registration was already committed.".into());
        }
        if self
            .saved_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.state.get() != SavedState::Prepared)
        {
            return Err("The saved Word registration is no longer prepared.".into());
        }
        let current = self.docs.media_views.borrow().get(&self.view).cloned();
        let same = match (&current, &self.previous) {
            (None, None) => true,
            (Some(current), Some(previous)) => {
                current.same_view(previous) && current.owner.same_owner(&previous.owner)
            }
            _ => false,
        };
        if !same
            || (self.path.is_some() && self.docs.window_opens.get(self.view) != self.expected_path)
            || self.docs.torn_down.borrow().contains(&self.view) != self.was_torn_down
        {
            return Err("The document registration state changed.".into());
        }
        let owner = match (&self.previous, successor) {
            (None, None) if transition.matches_owner(&self.owner) => self.owner.clone(),
            (Some(previous), Some(next))
                if previous.owner.same_owner(&self.owner)
                    && transition.matches_owner(&previous.owner)
                    && transition.proposes_owner(next) =>
            {
                next.clone()
            }
            _ => return Err("The document browser owner changed.".into()),
        };
        Ok(DocsValidatedRegistration {
            prepared: self,
            owner,
            _transition: std::marker::PhantomData,
        })
    }

    pub fn commit_admitted(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        successor: Option<&shell_contract::ipc::NativeBrowserOwner>,
    ) -> Result<(), String> {
        self.validate_in_transition(transition, successor)?.commit();
        Ok(())
    }

    /// Performs revocation and test binding after the native admission has ended.
    pub fn finish(mut self) -> Result<DocsRegisteredView, String> {
        if !self.committed {
            return Err("The document registration was not committed.".into());
        }
        if let Some(previous) = self.previous.take() {
            previous.mark_replaced();
            let permit = previous.last_permit();
            let pending = previous.pending();
            previous.revoke();
            let _ = self.docs.engine.forget_lazy_media_owner(permit);
            if let Some(pending) = pending {
                let _ = self.docs.engine.forget_lazy_media_owner(pending);
            }
        }
        let receipt = DocsRegisteredView {
            docs: Rc::clone(&self.docs),
            view: self.view,
            created: self.created.clone(),
            reservation: self.reservation.clone(),
            owner: self
                .committed_owner
                .take()
                .ok_or("The document browser owner was not committed.")?,
            lease: self.lease.clone(),
            initial_permit: self.initial_permit.clone(),
            registered_path: self.registered_path.take(),
            closed: Cell::new(false),
        };
        #[cfg(any(test, feature = "probe"))]
        if let Some(permit) = self.lease.current() {
            match self.docs.synthetic_media_binding(self.view, &permit) {
                Ok(Some(binding)) => {
                    let previous = self
                        .docs
                        .synthetic_media
                        .borrow_mut()
                        .insert(self.view, binding);
                    drop(previous);
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = receipt.discard();
                    return Err(error);
                }
            }
        }
        Ok(receipt)
    }
}
impl DocsValidatedRegistration<'_> {
    pub fn commit(self) {
        let prepared = self.prepared;
        prepared.docs.media_views.borrow_mut().insert(
            prepared.view,
            super::DocsMediaSlot {
                lease: prepared.lease.clone(),
                owner: self.owner.clone(),
                cleanup: prepared.cleanup.take(),
                retirement: prepared.retirement.clone(),
            },
        );
        prepared.committed_owner = Some(self.owner);
        if let Some(path) = prepared.path.take() {
            prepared.displaced_path = prepared.docs.window_opens.insert(prepared.view, path);
        }
        prepared.docs.torn_down.borrow_mut().remove(&prepared.view);
        prepared.committed = true;
        if let Some(receipt) = &prepared.saved_receipt {
            receipt.state.set(SavedState::Committed);
        }
    }
}

impl DocsRegisteredView {
    pub fn retire_closed(
        &self,
        receipt: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !receipt.matches(&self.reservation, self.view, &self.created) {
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
        if self.discard()? {
            Ok(())
        } else {
            Err("The document actor was replaced or is still in use.".into())
        }
    }
    fn validate_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, &self.owner) {
            return Err("The supersession belongs to another document owner.".into());
        }
        let replaced = self
            .docs
            .media_views
            .borrow()
            .get(&self.view)
            .is_some_and(|slot| {
                proof.successor_matches(&slot.owner) && !slot.same_view(&self.lease)
            });
        if !replaced {
            return Err("The successor document actor was not committed.".into());
        }
        Ok(())
    }
    pub fn retire_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        self.validate_superseded(proof)?;
        self.lease.mark_replaced();
        let permit = self.lease.last_permit();
        let pending = self.lease.pending();
        self.lease.revoke();
        self.docs.engine.forget_lazy_media_owner(permit)?;
        if let Some(pending) = pending {
            self.docs.engine.forget_lazy_media_owner(pending)?;
        }
        Ok(())
    }
    pub fn lease(&self) -> MediaViewLease {
        self.lease.clone()
    }

    pub fn discard(&self) -> Result<bool, String> {
        match self.lease.retirement() {
            main_core::docs::byte_handoff::MediaViewRetirement::Retired => return Ok(true),
            main_core::docs::byte_handoff::MediaViewRetirement::Replaced => return Ok(false),
            main_core::docs::byte_handoff::MediaViewRetirement::Active => {}
        }
        if self
            .docs
            .services
            .views
            .renderer_generation(self.view)
            .is_some_and(|actual| !actual.same_generation(&self.created))
        {
            return Ok(false);
        }
        if self.closed.get() && self.docs.media_views.borrow().get(&self.view).is_none() {
            self.lease.mark_retired();
            return Ok(true);
        }
        let same = self
            .docs
            .media_views
            .borrow()
            .get(&self.view)
            .is_some_and(|actual| {
                actual.same_view(&self.lease) && actual.owner.same_owner(&self.owner)
            });
        if !same
            || !self
                .lease
                .current()
                .is_some_and(|permit| permit.same_session(&self.initial_permit))
            || self.lease.pending().is_some()
            || self
                .registered_path
                .as_ref()
                .is_some_and(|path| self.docs.window_opens.get(self.view).as_ref() != Some(path))
        {
            return Ok(false);
        }
        let removed = self.docs.media_views.borrow_mut().remove(&self.view);
        if let Some(lease) = &removed {
            lease.mark_retired();
        }
        if self.registered_path.is_some() {
            self.docs.window_opens.remove(self.view);
        }
        #[cfg(any(test, feature = "probe"))]
        let binding = self.docs.synthetic_media.borrow_mut().remove(&self.view);
        if let Some(lease) = removed {
            let permit = lease.last_permit();
            lease.revoke();
            let _ = self.docs.engine.forget_lazy_media_owner(permit);
        }
        #[cfg(any(test, feature = "probe"))]
        drop(binding);
        Ok(true)
    }
}
