use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use main_core::packages::electron_utils::generate_handoff::{
    ConvertedEventReceipt, GenerateConvertKind, GenerateConvertTarget, GenerateHandoff,
    GenerateHandoffTarget, GenerateQueueReceipt, GenerateRunStarted, HandoffStore,
};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, NativeRequestOwner};
use shell_contract::runtime::{CancelToken, RendererGeneration};
use shell_contract::{Provided, ViewId};

#[derive(Clone)]
struct Ownership {
    identity: Rc<Cell<bool>>,
    created: RendererGeneration,
    effects: OwnedEffectsHost,
    // This proof records the issuer; accepted payloads never reuse its live authority.
    _source_provenance: NativeRequestOwner,
}

#[derive(Default)]
pub(super) struct Owners(HashMap<ViewId, Ownership>);

pub(super) fn owners(provided: &Provided) -> Rc<RefCell<Owners>> {
    if let Some(owners) = provided.all::<RefCell<Owners>>().into_iter().next() {
        return owners;
    }
    let owners = Rc::new(RefCell::new(Owners::default()));
    provided.add::<RefCell<Owners>>(owners.clone());
    owners
}

pub struct GenerateRollback {
    view: ViewId,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: Option<shell_contract::ipc::NativeBrowserOwner>,
    identity: Rc<Cell<bool>>,
    owners: Rc<RefCell<Owners>>,
    store: Rc<RefCell<HandoffStore>>,
    queue: RefCell<GenerateQueueReceipt>,
    displaced: RefCell<Option<Ownership>>,
    done: Cell<bool>,
    closed: Cell<bool>,
}
impl GenerateRollback {
    pub fn rollback(&self) -> Result<(), String> {
        if self.done.get() {
            return Ok(());
        }
        if self.queue.borrow().was_consumed() {
            self.done.set(true);
            return Ok(());
        }
        if !self.created.is_active() && !self.closed.get() {
            return Err("The generate handoff lacks an exact close proof".into());
        }
        let mut owners = self.owners.borrow_mut();
        if !owners
            .0
            .get(&self.view)
            .is_some_and(|owner| Rc::ptr_eq(&owner.identity, &self.identity))
        {
            return Err("The generate handoff ownership was replaced before rollback".into());
        }
        self.store
            .borrow_mut()
            .rollback_generate_handoff(&mut self.queue.borrow_mut())?;
        owners.0.remove(&self.view);
        if let Some(previous) = self
            .displaced
            .borrow_mut()
            .take()
            .filter(|owner| owner.identity.get())
        {
            owners.0.insert(self.view, previous);
        }
        self.done.set(true);
        Ok(())
    }
    pub fn retire_closed(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) {
            return Err("The generate handoff close proof does not match".into());
        }
        self.closed.set(true);
        Ok(())
    }
    pub fn retire_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        if !self
            .browser
            .as_ref()
            .is_some_and(|browser| proof.matches(&self.reservation, browser))
        {
            return Err("The generate handoff supersession proof does not match".into());
        }
        let mut owners = self.owners.borrow_mut();
        self.store
            .borrow_mut()
            .retire_superseded_handoff(&mut self.queue.borrow_mut());
        if owners
            .0
            .get(&self.view)
            .is_some_and(|owner| Rc::ptr_eq(&owner.identity, &self.identity))
        {
            owners.0.remove(&self.view);
        }
        self.identity.set(false);
        self.displaced.borrow_mut().take();
        self.done.set(true);
        Ok(())
    }

    pub fn retire_adopted(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created)
            || proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed
        {
            return Err("The adopted generate close proof does not match".into());
        }
        if self.done.get() && !self.queue.borrow().was_consumed() {
            return Ok(());
        }
        let consumed = self.queue.borrow().was_consumed();
        let mut owners = self.owners.borrow_mut();
        let current = owners
            .0
            .get(&self.view)
            .is_some_and(|owner| Rc::ptr_eq(&owner.identity, &self.identity));
        if !current && !consumed {
            return Err("The adopted generate ownership was replaced".into());
        }
        self.store
            .borrow_mut()
            .retire_adopted_handoff(&mut self.queue.borrow_mut())?;
        if current {
            owners.0.remove(&self.view);
        }
        self.done.set(true);
        Ok(())
    }
}

pub struct ConvertedRollback {
    view: ViewId,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: Option<shell_contract::ipc::NativeBrowserOwner>,
    identity: Rc<Cell<bool>>,
    owners: Rc<RefCell<Owners>>,
    store: Rc<RefCell<HandoffStore>>,
    event: RefCell<ConvertedEventReceipt>,
    displaced: RefCell<Option<Ownership>>,
    done: Cell<bool>,
    closed: Cell<bool>,
}
impl ConvertedRollback {
    pub fn rollback(&self) -> Result<(), String> {
        if self.done.get() {
            return Ok(());
        }
        if !self.created.is_active() && !self.closed.get() {
            return Err("The converted handoff lacks an exact close proof".into());
        }
        let mut owners = self.owners.borrow_mut();
        if !owners
            .0
            .get(&self.view)
            .is_some_and(|owner| Rc::ptr_eq(&owner.identity, &self.identity))
        {
            return Err("The converted handoff ownership was replaced before rollback".into());
        }
        self.store
            .borrow_mut()
            .rollback_generate_converted(&mut self.event.borrow_mut())?;
        owners.0.remove(&self.view);
        if let Some(previous) = self
            .displaced
            .borrow_mut()
            .take()
            .filter(|owner| owner.identity.get())
        {
            owners.0.insert(self.view, previous);
        }
        self.done.set(true);
        Ok(())
    }
    pub fn retire_closed(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) {
            return Err("The converted handoff close proof does not match".into());
        }
        self.closed.set(true);
        Ok(())
    }
    pub fn retire_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        if !self
            .browser
            .as_ref()
            .is_some_and(|browser| proof.matches(&self.reservation, browser))
        {
            return Err("The converted handoff supersession proof does not match".into());
        }
        let mut owners = self.owners.borrow_mut();
        if owners
            .0
            .get(&self.view)
            .is_some_and(|owner| Rc::ptr_eq(&owner.identity, &self.identity))
        {
            owners.0.remove(&self.view);
        }
        self.identity.set(false);
        self.displaced.borrow_mut().take();
        self.done.set(true);
        Ok(())
    }

    pub fn retire_adopted(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
        consumed: bool,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created)
            || proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed
        {
            return Err("The adopted conversion close proof does not match".into());
        }
        if self.done.get() {
            return Ok(());
        }
        let mut owners = self.owners.borrow_mut();
        if owners
            .0
            .get(&self.view)
            .is_some_and(|owner| Rc::ptr_eq(&owner.identity, &self.identity))
        {
            owners.0.remove(&self.view);
        } else if !consumed {
            return Err("The adopted conversion ownership was replaced".into());
        }
        self.done.set(true);
        Ok(())
    }
}

fn controlled<T>(
    admit: impl FnOnce(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let mut work = Some(work);
    let mut output = None;
    let admitted = admit(&mut || {
        let work = work
            .take()
            .ok_or("handoff effect was admitted more than once")?;
        let result = work();
        let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
        output = Some(result);
        status
    });
    match (admitted, output) {
        (_, Some(Err(error))) => Err(error),
        (Ok(()), Some(Ok(value))) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(()), None) => Err("handoff effect was not admitted".into()),
    }
}

pub fn handoff_consumer(cx: &CallContext) -> Result<NativeRequestOwner, String> {
    let context = cx
        .native
        .clone()
        .ok_or("The handoff request has no authenticated native owner")?;
    Ok(NativeRequestOwner {
        view: cx.sender.view,
        context,
    })
}

pub struct PreparedGenerateHandoff {
    view: ViewId,
    queue: main_core::packages::electron_utils::generate_handoff::PreparedGenerateQueue,
    source: NativeRequestOwner,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: shell_contract::ipc::NativeBrowserOwner,
    effects: OwnedEffectsHost,
    identity: Rc<Cell<bool>>,
    owners: Rc<RefCell<Owners>>,
    store: Rc<RefCell<HandoffStore>>,
}

pub fn prepare_generate_handoff(
    provided: &Provided,
    view: ViewId,
    handoff: GenerateHandoff,
    source: NativeRequestOwner,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: shell_contract::ipc::NativeBrowserOwner,
    effects: OwnedEffectsHost,
) -> Result<PreparedGenerateHandoff, String> {
    if !reservation.matches(view, &created) {
        return Err("The generate handoff reservation does not match".into());
    }
    let owners = owners(provided);
    owners
        .borrow_mut()
        .0
        .try_reserve(1)
        .map_err(|error| error.to_string())?;
    let store = super::shared_handoff_store(provided);
    let queue = store
        .borrow_mut()
        .prepare_generate_queue(f64::from(view.0), handoff)?;
    Ok(PreparedGenerateHandoff {
        view,
        queue,
        source,
        created,
        reservation,
        browser,
        effects,
        identity: Rc::new(Cell::new(true)),
        owners,
        store,
    })
}

impl PreparedGenerateHandoff {
    pub fn commit(
        self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<GenerateRollback, String> {
        if !transition.matches(&self.reservation)
            || !(transition.matches_owner(&self.browser)
                || transition.proposes_owner(&self.browser))
        {
            return Err("The generate handoff browser transition does not match".into());
        }
        let mut metadata = self.owners.borrow_mut();
        let mut store = self.store.borrow_mut();
        if metadata.0.capacity() == metadata.0.len() && !metadata.0.contains_key(&self.view)
            || !store.can_commit_generate_queue(&self.queue)
        {
            return Err("The generate handoff capacity was consumed".into());
        }
        initialize()?;
        let queue = store.commit_generate_queue(self.queue);
        let displaced = metadata.0.insert(
            self.view,
            Ownership {
                identity: self.identity.clone(),
                created: self.created.clone(),
                effects: self.effects,
                _source_provenance: self.source,
            },
        );
        drop(store);
        drop(metadata);
        Ok(GenerateRollback {
            view: self.view,
            created: self.created,
            reservation: self.reservation,
            browser: Some(self.browser),
            identity: self.identity,
            owners: self.owners,
            store: self.store,
            queue: RefCell::new(queue),
            displaced: RefCell::new(displaced),
            done: Cell::new(false),
            closed: Cell::new(false),
        })
    }
}

#[cfg(test)]
/// Called at constructed-target admission, before load. Success transfers the immutable request.
pub fn queue_generate_handoff_owned(
    provided: &Provided,
    view: ViewId,
    handoff: GenerateHandoff,
    source: NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: CancelToken,
    pending: CancelToken,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    effects: OwnedEffectsHost,
    initialize: &mut dyn FnMut() -> Result<(), String>,
) -> Result<GenerateRollback, String> {
    if !reservation.matches(view, &created) {
        return Err("The generate handoff reservation does not match".into());
    }
    let metadata = owners(provided);
    let store = super::shared_handoff_store(provided);
    let identity = Rc::new(Cell::new(true));
    let (queue, displaced) = controlled(
        |effect| {
            effects.with_created_many_effect(
                &source,
                participants,
                &created,
                &cancel,
                &pending,
                effect,
            )
        },
        || {
            initialize()?;
            let queue = store
                .borrow_mut()
                .queue_generate_handoff_reversible(f64::from(view.0), handoff);
            let displaced = metadata.borrow_mut().0.insert(
                view,
                Ownership {
                    identity: identity.clone(),
                    created: created.clone(),
                    effects: effects.clone(),
                    _source_provenance: source.clone(),
                },
            );
            Ok((queue, displaced))
        },
    )?;
    Ok(GenerateRollback {
        view,
        created,
        reservation,
        browser: None,
        identity,
        owners: metadata,
        store,
        queue: RefCell::new(queue),
        displaced: RefCell::new(displaced),
        done: Cell::new(false),
        closed: Cell::new(false),
    })
}

fn authority(
    provided: &Provided,
    consumer: &NativeRequestOwner,
) -> Result<Option<Ownership>, String> {
    let metadata = owners(provided);
    let ownership = metadata.borrow().0.get(&consumer.view).cloned();
    let Some(ownership) = ownership else {
        return Ok(None);
    };
    let renderer = consumer
        .context
        .renderer()
        .ok_or("The handoff consumer has no authenticated renderer")?;
    if !renderer.same_generation(&ownership.created) {
        return Err("The handoff belongs to another renderer lifetime".into());
    }
    Ok(Some(ownership))
}

pub fn take_generate_handoff_owned(
    provided: &Provided,
    target: GenerateHandoffTarget,
    consumer: &NativeRequestOwner,
    cancel: &CancelToken,
) -> Result<Option<GenerateHandoff>, String> {
    let Some(authority) = authority(provided, consumer)? else {
        return Ok(None);
    };
    let store = super::shared_handoff_store(provided);
    let metadata = owners(provided);
    controlled(
        |effect| {
            authority
                .effects
                .with_owned_effect(consumer, consumer, cancel, cancel, effect)
        },
        || {
            if !metadata
                .borrow()
                .0
                .get(&consumer.view)
                .is_some_and(|current| Rc::ptr_eq(&current.identity, &authority.identity))
            {
                return Err("The handoff ownership changed before consumption".into());
            }
            Ok(store
                .borrow_mut()
                .take_generate_handoff(target, f64::from(consumer.view.0)))
        },
    )
}

pub fn report_generate_run_started_owned(
    provided: &Provided,
    target: GenerateHandoffTarget,
    consumer: &NativeRequestOwner,
    cancel: &CancelToken,
    attached_paths: &[String],
    now_ms: f64,
) -> Result<Option<GenerateRunStarted>, String> {
    let Some(authority) = authority(provided, consumer)? else {
        return Ok(None);
    };
    let store = super::shared_handoff_store(provided);
    let metadata = owners(provided);
    let mut retired = None;
    let result = controlled(
        |effect| {
            authority
                .effects
                .with_owned_effect(consumer, consumer, cancel, cancel, effect)
        },
        || {
            if !metadata
                .borrow()
                .0
                .get(&consumer.view)
                .is_some_and(|current| Rc::ptr_eq(&current.identity, &authority.identity))
            {
                return Err("The handoff ownership changed before its run was recorded".into());
            }
            let result = store.borrow_mut().report_generate_run_started(
                target,
                f64::from(consumer.view.0),
                attached_paths,
                now_ms,
            );
            if !matches!(result, Ok(None)) {
                let mut owners = metadata.borrow_mut();
                if owners
                    .0
                    .get(&consumer.view)
                    .is_some_and(|current| Rc::ptr_eq(&current.identity, &authority.identity))
                {
                    retired = owners.0.remove(&consumer.view);
                }
            }
            result
        },
    );
    drop(retired);
    result
}

pub struct PreparedConverted {
    view: ViewId,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: shell_contract::ipc::NativeBrowserOwner,
    source: NativeRequestOwner,
    effects: OwnedEffectsHost,
    identity: Rc<Cell<bool>>,
    owners: Rc<RefCell<Owners>>,
    store: Rc<RefCell<HandoffStore>>,
    event: main_core::packages::electron_utils::generate_handoff::PreparedConvertedEvent,
}

pub fn prepare_generate_converted(
    provided: &Provided,
    view: ViewId,
    target: GenerateConvertTarget,
    kind: GenerateConvertKind,
    sources: &[String],
    id: &str,
    now_ms: f64,
    source: NativeRequestOwner,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: shell_contract::ipc::NativeBrowserOwner,
    effects: OwnedEffectsHost,
) -> Result<PreparedConverted, String> {
    if !reservation.matches(view, &created) {
        return Err("The converted handoff reservation does not match".into());
    }
    let store = super::shared_handoff_store(provided);
    let event = store.borrow_mut().prepare_generate_converted(
        f64::from(view.0),
        target,
        kind,
        sources,
        id,
        now_ms,
    )?;
    let owners = owners(provided);
    owners
        .borrow_mut()
        .0
        .try_reserve(1)
        .map_err(|error| error.to_string())?;
    Ok(PreparedConverted {
        view,
        created,
        reservation,
        browser,
        source,
        effects,
        identity: Rc::new(Cell::new(true)),
        owners,
        store,
        event,
    })
}

impl PreparedConverted {
    pub fn commit(
        self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<ConvertedRollback, String> {
        if !transition.matches(&self.reservation)
            || !(transition.matches_owner(&self.browser)
                || transition.proposes_owner(&self.browser))
        {
            return Err("The converted handoff browser transition does not match".into());
        }
        let mut owners = self.owners.borrow_mut();
        let mut store = self.store.borrow_mut();
        if (owners.0.capacity() == owners.0.len() && !owners.0.contains_key(&self.view))
            || !store.can_commit_generate_converted()
        {
            return Err("The converted handoff capacity was consumed".into());
        }
        initialize()?;
        let (_, event) = store.commit_generate_converted(self.event);
        let displaced = owners.0.insert(
            self.view,
            Ownership {
                identity: self.identity.clone(),
                created: self.created.clone(),
                effects: self.effects,
                _source_provenance: self.source,
            },
        );
        drop(store);
        drop(owners);
        Ok(ConvertedRollback {
            view: self.view,
            created: self.created,
            reservation: self.reservation,
            browser: Some(self.browser),
            identity: self.identity,
            owners: self.owners,
            store: self.store,
            event: RefCell::new(event),
            displaced: RefCell::new(displaced),
            done: Cell::new(false),
            closed: Cell::new(false),
        })
    }
}

#[cfg(test)]
/// Only an actor's admitted initializer calls this bounded state commit, before publishing its seed.
pub fn record_generate_converted_fenced(
    provided: &Provided,
    view: ViewId,
    target: GenerateConvertTarget,
    kind: GenerateConvertKind,
    sources: &[String],
    id: &str,
    now_ms: f64,
    source: NativeRequestOwner,
    created: RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    effects: OwnedEffectsHost,
) -> Result<ConvertedRollback, String> {
    if !reservation.matches(view, &created) {
        return Err("The converted handoff reservation does not match".into());
    }
    let store = super::shared_handoff_store(provided);
    let owners = owners(provided);
    let (_, event) = store.borrow_mut().record_generate_converted_reversible(
        f64::from(view.0),
        target,
        kind,
        sources,
        id,
        now_ms,
    )?;
    let identity = Rc::new(Cell::new(true));
    let displaced = owners.borrow_mut().0.insert(
        view,
        Ownership {
            identity: identity.clone(),
            created: created.clone(),
            effects,
            _source_provenance: source,
        },
    );
    Ok(ConvertedRollback {
        view,
        created,
        reservation,
        browser: None,
        identity,
        owners,
        store,
        event: RefCell::new(event),
        displaced: RefCell::new(displaced),
        done: Cell::new(false),
        closed: Cell::new(false),
    })
}

pub fn drop_generate_handoff_owned(
    provided: &Provided,
    view: ViewId,
    expected: &RendererGeneration,
) {
    let metadata = owners(provided);
    let mut metadata = metadata.borrow_mut();
    if metadata
        .0
        .get(&view)
        .is_some_and(|ownership| ownership.created.same_generation(expected))
    {
        metadata.0.remove(&view);
        super::shared_handoff_store(provided)
            .borrow_mut()
            .drop_generate_handoff(f64::from(view.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::packages::electron_utils::generate_handoff::Material;
    use shell_contract::hosts::OwnedEffects;
    use shell_contract::ipc::NativeContext;
    use std::sync::Arc;
    fn queue_generate_handoff_owned(
        provided: &Provided,
        view: ViewId,
        handoff: GenerateHandoff,
        source: NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: CancelToken,
        pending: CancelToken,
        created: RendererGeneration,
        effects: OwnedEffectsHost,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<GenerateRollback, String> {
        let (_, reservation) = shell_contract::ipc::NativeClosureIssuer::new(view, created.clone());
        super::queue_generate_handoff_owned(
            provided,
            view,
            handoff,
            source,
            participants,
            cancel,
            pending,
            created,
            reservation,
            effects,
            initialize,
        )
    }

    #[derive(Default)]
    struct Effects {
        events: std::sync::Mutex<Vec<(String, Vec<shell_contract::IpcValue>)>>,
    }
    impl OwnedEffects for Effects {
        fn capture_owner(
            &self,
            _: ViewId,
            _: &RendererGeneration,
        ) -> Result<NativeRequestOwner, String> {
            panic!("no current-owner lookup in handoff tests")
        }
        fn docs_document_identity(&self, owner: &NativeRequestOwner) -> Option<(ViewId, u64)> {
            owner.context.downcast_ref::<(ViewId, u64)>().copied()
        }
        fn with_native_command_preparation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native command preparation is not scripted in the handoff fixture".into())
        }
        fn with_startup_command_creation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &RendererGeneration,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup command creation is not scripted in the handoff fixture".into())
        }
        fn with_startup_command_target_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &shell_contract::ipc::NativeInvocationDocument,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup command target is not scripted in the handoff fixture".into())
        }
        fn with_startup_command_preparation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup command preparation is not scripted in the handoff fixture".into())
        }
        fn with_startup_recovery_creation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &RendererGeneration,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup recovery creation is not scripted in the handoff fixture".into())
        }
        fn with_startup_recovery_seed_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationTarget,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup recovery seed is not scripted in the handoff fixture".into())
        }
        fn with_native_command_creation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &RendererGeneration,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native command creation is not scripted in the handoff fixture".into())
        }
        fn with_native_command_target_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &shell_contract::ipc::NativeInvocationDocument,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native command target is not scripted in the handoff fixture".into())
        }
        fn settle_recovery_seed_snapshot(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery settlement is not scripted in the handoff fixture".into())
        }
        fn emit_recovery_open_snapshot(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Recovery events are not scripted in the handoff fixture".into())
        }
        fn with_recovery_dialog_snapshot(
            &self,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &RendererGeneration,
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery dialog is not scripted in the handoff fixture".into())
        }
        fn emit_recovery_dialog_snapshot(
            &self,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &RendererGeneration,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Recovery dialog events are not scripted in the handoff fixture".into())
        }
        fn post_ui_existing_document_save(
            &self,
            _: shell_contract::ipc::NativeExistingDocumentBinding,
            _: Vec<NativeRequestOwner>,
            _: CancelToken,
            _: CancelToken,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            done(Err(
                "Native existing-document save is not scripted in the handoff fixture".into(),
            ));
        }
        fn with_existing_document_save_effect(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native existing-document save is not scripted in the handoff fixture".into())
        }
        fn with_existing_document_save_effect_extra(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native existing-document save is not scripted in the handoff fixture".into())
        }
        fn with_existing_document_same_origin_effect(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native existing-document save is not scripted in the handoff fixture".into())
        }
        fn emit_existing_document_save_many_extra(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Native existing-document save is not scripted in the handoff fixture".into())
        }
        fn post_ui_existing_document_save_extra(
            &self,
            _: shell_contract::ipc::NativeExistingDocumentBinding,
            _: Vec<NativeRequestOwner>,
            _: CancelToken,
            _: CancelToken,
            _: Vec<CancelToken>,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            done(Err(
                "Native existing-document save is not scripted in the handoff fixture".into(),
            ));
        }
        fn with_owned_effect_many_extra(
            &self,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Extra-cancellation ownership is not scripted in the handoff fixture".into())
        }
        fn emit_owned_many_extra(
            &self,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Extra-cancellation events are not scripted in the handoff fixture".into())
        }
        fn post_ui_owned_many_extra(
            &self,
            _: NativeRequestOwner,
            _: Vec<NativeRequestOwner>,
            _: CancelToken,
            _: CancelToken,
            _: Vec<CancelToken>,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            done(Err(
                "Extra-cancellation UI is not scripted in the handoff fixture".into(),
            ));
        }
        fn with_native_invocation_effect_extra(
            &self,
            _: &shell_contract::ipc::NativeInvocationDocument,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native invocation is not scripted in the handoff fixture".into())
        }
        fn with_native_save_reload_effect(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadReceipt,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the handoff fixture".into())
        }
        fn with_native_save_reload_many_effect_extra(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the handoff fixture".into())
        }
        fn stage_saved_docs_media(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &dyn shell_contract::hosts::DocsMediaStage,
        ) -> Result<(), String> {
            Err("Saved Docs media is not scripted in the handoff fixture".into())
        }
        fn post_ui_native_saved_document_input(
            &self,
            _: shell_contract::ipc::NativeSavedDocument,
            _: Vec<NativeRequestOwner>,
            _: CancelToken,
            _: CancelToken,
            _: Vec<CancelToken>,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            done(Err(
                "Saved Docs UI is not scripted in the handoff fixture".into()
            ));
        }
        fn with_prepared_saved_publication(
            &self,
            _: &shell_contract::ipc::SavedPublication,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(
                &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Saved publication is not scripted in the handoff fixture".into())
        }
        fn transition_saved_docs_media(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &dyn shell_contract::hosts::DocsMediaTransition,
            _: &mut dyn FnMut(
                &NativeRequestOwner,
                &mut dyn FnMut() -> Result<(), String>,
            ) -> Result<(), String>,
            _: &mut dyn FnMut(
                &shell_contract::ipc::NativeSavedDocument,
                &NativeRequestOwner,
            ) -> Result<(), String>,
        ) -> Result<(shell_contract::ipc::NativeSavedDocument, NativeRequestOwner), String>
        {
            Err("Saved Docs media is not scripted in the handoff fixture".into())
        }
        fn with_native_save_reload_completion_effect(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadHandoff,
            _: &NativeRequestOwner,
            _: &mut dyn FnMut(),
            _: &mut dyn FnMut(
                &mut shell_contract::ipc::AdmittedPdfReloadRetirement<'_, '_>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the handoff fixture".into())
        }
        fn with_native_save_reload_parser_effect(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadHandoff,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload parser is not scripted in the handoff fixture".into())
        }
        fn with_native_save_reload_reader_effect(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadReceipt,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload reader is not scripted in the handoff fixture".into())
        }
        fn with_saved_reload_acceptance_effect(
            &self,
            _: &shell_contract::ipc::NativeSavedInput,
            _: &shell_contract::ipc::NativeSavedPath,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Saved reload acceptance is not scripted in the handoff fixture".into())
        }
        fn with_native_save_revision_transition(
            &self,
            _: &mut shell_contract::ipc::PreparedNativeSaveRevision,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(),
            _: &mut dyn FnMut(
                &mut shell_contract::ipc::AdmittedPdfRevisionSet<'_, '_>,
                &mut shell_contract::ipc::PreparedNativeSaveRevision,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save revision is not scripted in the handoff fixture".into())
        }
        fn with_native_saved_target_seed(
            &self,
            _: &shell_contract::ipc::NativeSavedTarget,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native saved target is not scripted in the handoff fixture".into())
        }
        fn with_native_saved_document_input(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native saved document is not scripted in the handoff fixture".into())
        }
        fn emit_native_saved_document_input(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Native saved document events are not scripted in the handoff fixture".into())
        }
        fn with_native_saved_document_same_origin(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native saved document is not scripted in the handoff fixture".into())
        }
        fn emit_native_saved_document_same_origin(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Native saved document events are not scripted in the handoff fixture".into())
        }
        fn with_native_saved_document_transition(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(
                &shell_contract::ipc::NativeSavedInput,
                &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native saved document transition is not scripted in the handoff fixture".into())
        }
        fn emit_native_save_revision_snapshot(
            &self,
            _: &shell_contract::ipc::NativeSaveRevisionSnapshot,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Native save revision events are not scripted in the handoff fixture".into())
        }
        fn post_ui_native_save_revision_snapshot(
            &self,
            _: shell_contract::ipc::NativeSaveRevisionSnapshot,
            _: Vec<NativeRequestOwner>,
            _: CancelToken,
            _: CancelToken,
            _: Vec<CancelToken>,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            done(Err(
                "Native save revision UI is not scripted in the handoff fixture".into(),
            ));
        }
        fn emit_existing_document_save_many(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err(
                "Native existing-document save events are not scripted in the handoff fixture"
                    .into(),
            )
        }
        fn with_recovery_seed_snapshot(
            &self,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery seed is not scripted in the handoff fixture".into())
        }
        fn commit_recovery_seed_snapshot(
            &self,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
            Err("Recovery seed is not scripted in the handoff fixture".into())
        }
        fn with_recovery_open_snapshot(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery open is not scripted in the handoff fixture".into())
        }
        fn with_command_recovery_target_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeInvocationDocument,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Command recovery is not scripted in the handoff fixture".into())
        }
        fn with_recovery_pdf_revision_transition(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &shell_contract::ipc::PdfDocumentLease,
            _: &shell_contract::ipc::PdfDocumentLease,
            _: &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal,
            _: &mut dyn FnMut(),
            _: &mut dyn FnMut(
                &mut shell_contract::ipc::AdmittedPdfRevisionRetirement<'_, '_>,
                &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery PDF revision transition is not scripted in the handoff fixture".into())
        }
        fn with_recovery_open_effect_many_extra(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery open is not scripted in the handoff fixture".into())
        }
        fn with_recovery_open_effect_many(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery open is not scripted in the handoff fixture".into())
        }
        fn emit_native_command_creation_many(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &RendererGeneration,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Native command creation events are not scripted in the handoff fixture".into())
        }
        fn emit_recovery_open_many(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Recovery events are not scripted in the handoff fixture".into())
        }
        fn stage_docs_media(
            &self,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &dyn shell_contract::hosts::DocsMediaStage,
            _: Option<&shell_contract::ipc::RecoverySeedReceipt>,
        ) -> Result<(), String> {
            Err("Docs media staging is not scripted in the handoff fixture".into())
        }
        fn with_startup_recovery_effect(
            &self,
            startup: &shell_contract::ipc::NativeInvocationOperation,
            shell: &NativeRequestOwner,
            child: &shell_contract::ipc::NativeInvocationDocument,
            owner: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_startup_recovery(
                startup,
                shell,
                child,
                owner,
                participants,
                cancel,
                pending,
                effect,
            )
        }
        fn emit_startup_recovery_many(
            &self,
            startup: &shell_contract::ipc::NativeInvocationOperation,
            shell: &NativeRequestOwner,
            child: &shell_contract::ipc::NativeInvocationDocument,
            owner: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            participants
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            shell_contract::hosts::synthetic::with_startup_recovery(
                startup,
                shell,
                child,
                owner,
                participants,
                cancel,
                pending,
                &mut || {
                    self.events
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn transition_docs_media(
            &self,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &dyn shell_contract::hosts::DocsMediaTransition,
            _: Option<&shell_contract::ipc::RecoverySeedReceipt>,
        ) -> Result<NativeRequestOwner, String> {
            Err("Docs media transition is not scripted in the handoff fixture".into())
        }
        fn with_native_operation_effect(
            &self,
            operation: &shell_contract::ipc::NativeInvocationOperation,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_native_operation(
                operation,
                origin,
                participants,
                cancel,
                pending,
                effect,
            )
        }
        fn emit_native_operation_many(
            &self,
            operation: &shell_contract::ipc::NativeInvocationOperation,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            participants
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            shell_contract::hosts::synthetic::with_native_operation(
                operation,
                origin,
                participants,
                cancel,
                pending,
                &mut || {
                    self.events
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn with_native_invocation_effect(
            &self,
            document: &shell_contract::ipc::NativeInvocationDocument,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_native_invocation(
                document,
                origin,
                participants,
                cancel,
                pending,
                effect,
            )
        }
        fn emit_native_invocation_many(
            &self,
            document: &shell_contract::ipc::NativeInvocationDocument,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            participants
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            shell_contract::hosts::synthetic::with_native_invocation(
                document,
                origin,
                participants,
                cancel,
                pending,
                &mut || {
                    self.events
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn with_same_native_origin_effect(
            &self,
            expected: &NativeRequestOwner,
            actual: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[&CancelToken],
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_same_native_origin(
                expected,
                actual,
                participants,
                cancel,
                pending,
                additional,
                effect,
            )
        }
        fn with_recovery_dialog_effect(
            &self,
            target: &shell_contract::ipc::RecoveryOpenTarget,
            dialog: &NativeRequestOwner,
            generation: &RendererGeneration,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_recovery_dialog(
                target, dialog, generation, cancel, pending, effect,
            )
        }
        fn emit_recovery_dialog(
            &self,
            target: &shell_contract::ipc::RecoveryOpenTarget,
            dialog: &NativeRequestOwner,
            generation: &RendererGeneration,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            let mut event = Some((channel.to_owned(), args));
            shell_contract::hosts::synthetic::with_recovery_dialog(
                target,
                dialog,
                generation,
                cancel,
                pending,
                &mut || {
                    self.events
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn with_recovery_seed_effect(
            &self,
            target: &shell_contract::ipc::RecoveryOpenTarget,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_recovery_seed(target, effect)
        }
        fn commit_recovery_seed(
            &self,
            target: &shell_contract::ipc::RecoveryOpenTarget,
            enqueue: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
            shell_contract::hosts::synthetic::commit_recovery_seed(target, enqueue)
        }
        fn with_recovery_open_effect_pending(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Pending recovery admission is not scripted in the handoff fixture".into())
        }
        fn with_recovery_open_effect(
            &self,
            receipt: &shell_contract::ipc::RecoverySeedReceipt,
            receiver: &NativeRequestOwner,
            cancel: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_recovery_open(receipt, receiver, cancel, effect)
        }
        fn settle_recovery_seed(
            &self,
            receipt: &shell_contract::ipc::RecoverySeedReceipt,
            receiver: &NativeRequestOwner,
            cancel: &CancelToken,
            result: Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::settle_recovery_seed(
                receipt, receiver, cancel, result,
            )
        }
        fn with_owned_transition(
            &self,
            source: &NativeRequestOwner,
            destination: &NativeRequestOwner,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut(
                &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            let source_loss = source.loss_token();
            let destination_loss = destination.loss_token();
            let documents = [
                source.context.document_lease(),
                destination.context.document_lease(),
            ];
            let mut notifications = [None, None];
            let result = RendererGeneration::with_active_all_retiring(
                [
                    source.context.renderer(),
                    destination.context.renderer(),
                    documents[0],
                    documents[1],
                ],
                documents,
                &mut notifications,
                |retirement| {
                    shell_contract::ipc::PdfDocumentLease::with_live_all(
                        [
                            source.pdf_document(),
                            source.pdf_revision(),
                            destination.pdf_document(),
                            destination.pdf_revision(),
                        ],
                        || {
                            CancelToken::with_active_all(
                                [
                                    Some(cancel),
                                    Some(pending),
                                    source_loss.as_ref(),
                                    destination_loss.as_ref(),
                                ],
                                || effect(retirement),
                            )
                        },
                    )
                },
            )
            .flatten()
            .flatten()
            .ok_or_else(|| "synthetic document ownership released".to_owned());
            for token in notifications.into_iter().flatten() {
                token.cancel();
            }
            result?
        }
        fn with_created_effect(
            &self,
            source: &NativeRequestOwner,
            created: &RendererGeneration,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            let loss = source.loss_token().unwrap();
            RendererGeneration::with_active_all([source.context.renderer(), Some(created)], || {
                CancelToken::with_active_all([Some(cancel), Some(pending), Some(&loss)], effect)
            })
            .flatten()
            .ok_or_else(|| "source or created renderer released".to_owned())?
        }
        fn with_created_many_effect(
            &self,
            source: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            created: &RendererGeneration,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_created_many(
                source,
                participants,
                created,
                cancel,
                pending,
                effect,
            )
        }
        fn with_owned_effect(
            &self,
            source: &NativeRequestOwner,
            destination: &NativeRequestOwner,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            let source_loss = source.loss_token().unwrap();
            let destination_loss = destination.loss_token().unwrap();
            RendererGeneration::with_active_all(
                [source.context.renderer(), destination.context.renderer()],
                || {
                    CancelToken::with_active_all(
                        [
                            Some(cancel),
                            Some(pending),
                            Some(&source_loss),
                            Some(&destination_loss),
                        ],
                        effect,
                    )
                },
            )
            .flatten()
            .ok_or_else(|| "source or consumer released".to_owned())?
        }
        fn with_owned_effect_many(
            &self,
            origin: &NativeRequestOwner,
            destinations: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            self.with_owned_transition_many(origin, destinations, cancel, pending, &mut |_| {
                effect()
            })
        }
        fn post_ui_recovery_open_many(
            &self,
            receipt: shell_contract::ipc::RecoverySeedReceipt,
            receiver: NativeRequestOwner,
            participants: Vec<NativeRequestOwner>,
            cancel: CancelToken,
            pending: CancelToken,
            action: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            let admitted = shell_contract::hosts::synthetic::with_recovery_open_many(
                &receipt,
                &receiver,
                &participants,
                &cancel,
                &pending,
                &mut || Ok(()),
            );
            done(admitted.and_then(|()| action()));
        }
        fn post_ui_owned_many(
            &self,
            origin: NativeRequestOwner,
            destinations: Vec<NativeRequestOwner>,
            cancel: CancelToken,
            pending: CancelToken,
            action: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            let admitted =
                self.with_owned_effect_many(&origin, &destinations, &cancel, &pending, &mut || {
                    Ok(())
                });
            done(admitted.and_then(|()| action()));
        }
        fn with_owned_transition_many(
            &self,
            origin: &NativeRequestOwner,
            destinations: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut(
                &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            let count = destinations.len() + 1;
            let mut generations = Vec::with_capacity(count * 2);
            let mut documents = Vec::with_capacity(count);
            let mut pdf = Vec::with_capacity(count * 2);
            let mut losses = Vec::with_capacity(count);
            for owner in std::iter::once(origin).chain(destinations) {
                generations.push(
                    owner
                        .context
                        .renderer()
                        .ok_or("synthetic renderer unavailable")?,
                );
                if let Some(document) = owner.context.document_lease() {
                    generations.push(document);
                    documents.push(document);
                }
                if let Some(document) = owner.pdf_document() {
                    pdf.push(document);
                }
                if let Some(revision) = owner.pdf_revision() {
                    pdf.push(revision);
                }
                losses.push(
                    owner
                        .loss_token()
                        .ok_or("synthetic owner loss unavailable")?,
                );
            }
            let mut tokens = Vec::with_capacity(losses.len() + 2);
            tokens.push(cancel);
            tokens.push(pending);
            tokens.extend(losses.iter());
            let mut generations =
                shell_contract::runtime::RendererGenerationGatePlan::from_generations(generations);
            let mut retirement = generations.prepare_retirement(&documents)?;
            let mut pdf = shell_contract::ipc::PdfDocumentGatePlan::new(&pdf);
            let mut tokens = shell_contract::runtime::CancelTokenGatePlan::from_tokens(tokens);
            generations
                .with_active_retiring(&mut retirement, |admitted| {
                    pdf.with_live(|| tokens.with_active(|| effect(admitted)))
                })
                .flatten()
                .flatten()
                .ok_or_else(|| "synthetic document ownership released".to_owned())?
        }
        fn emit_owned_many(
            &self,
            origin: &NativeRequestOwner,
            destinations: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            _channel: &str,
            _args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            destinations
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            self.with_owned_transition_many(origin, destinations, cancel, pending, &mut |_| {
                panic!("handoffs do not emit UI events")
            })
        }
        fn emit_owned(
            &self,
            _: &NativeRequestOwner,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            panic!("handoffs do not emit UI events")
        }
        fn post_ui_owned(
            &self,
            _: NativeRequestOwner,
            _: NativeRequestOwner,
            _: CancelToken,
            _: CancelToken,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            _: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            panic!("handoffs do not schedule UI work")
        }
    }

    fn owner(view: ViewId, renderer: RendererGeneration) -> NativeRequestOwner {
        NativeRequestOwner {
            view,
            context: NativeContext::with_owner_loss((), renderer, CancelToken::default()),
        }
    }
    fn handoff(id: &str) -> GenerateHandoff {
        GenerateHandoff {
            id: id.into(),
            mode: "make".into(),
            target: GenerateHandoffTarget::Mindmap,
            materials: vec![Material {
                path: "/source.md".into(),
                name: "source.md".into(),
            }],
            text: "make a map".into(),
            default_text: false,
            created_at: 1.0,
        }
    }

    #[test]
    fn superseded_handoff_is_not_restored_after_successor_rollback() {
        let provided = Provided::default();
        let view = ViewId(2);
        let source = owner(ViewId(1), RendererGeneration::new());
        let created = RendererGeneration::new();
        let effects: OwnedEffectsHost = Arc::new(Effects::default());
        let (_, reservation, browser) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, created.clone());
        let first = prepare_generate_handoff(
            &provided,
            view,
            handoff("first"),
            source.clone(),
            created.clone(),
            reservation.clone(),
            browser.clone(),
            effects.clone(),
        )
        .unwrap();
        let first = reservation
            .with_initial_owner(&browser, |transition| {
                first.commit(transition, &mut || Ok(()))
            })
            .unwrap();
        let replacement = reservation.prepare_replacement(&browser).unwrap();
        let second = prepare_generate_handoff(
            &provided,
            view,
            handoff("second"),
            source,
            created.clone(),
            reservation.clone(),
            replacement.successor().clone(),
            effects,
        )
        .unwrap();
        let (_, proof, second) = replacement
            .commit(|transition, _| second.commit(transition, &mut || Ok(())))
            .unwrap();
        first.retire_superseded(&proof).unwrap();
        second.rollback().unwrap();
        let actual = owner(view, created);
        assert!(
            take_generate_handoff_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &actual,
                &CancelToken::default()
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn another_renderer_cannot_take_or_destroy_the_valid_initial_handoff() {
        let provided = Provided::default();
        let source = owner(ViewId(1), RendererGeneration::new());
        let view = ViewId(2);
        let created = RendererGeneration::new();
        let cancel = CancelToken::default();
        queue_generate_handoff_owned(
            &provided,
            view,
            handoff("original"),
            source,
            &[],
            cancel.clone(),
            CancelToken::default(),
            created.clone(),
            Arc::new(Effects::default()),
            &mut || Ok(()),
        )
        .unwrap();
        let stale = RendererGeneration::new();
        let other = owner(view, stale.clone());
        assert!(
            take_generate_handoff_owned(&provided, GenerateHandoffTarget::Mindmap, &other, &cancel)
                .is_err()
        );
        drop_generate_handoff_owned(&provided, view, &stale);
        let actual = owner(view, created);
        let taken = take_generate_handoff_owned(
            &provided,
            GenerateHandoffTarget::Mindmap,
            &actual,
            &cancel,
        )
        .unwrap()
        .unwrap();
        assert_eq!(taken.id, "original");
        assert!(
            take_generate_handoff_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &actual,
                &cancel
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn normal_source_close_after_transfer_preserves_initial_take_and_result() {
        let provided = Provided::default();
        let source_renderer = RendererGeneration::new();
        let source = owner(ViewId(1), source_renderer.clone());
        let created = RendererGeneration::new();
        let actual = owner(ViewId(2), created.clone());
        let source_cancel = CancelToken::default();
        let consumer_cancel = CancelToken::default();
        let participant_renderer = RendererGeneration::new();
        let participant = owner(ViewId(3), participant_renderer.clone());
        let pending = CancelToken::default();
        queue_generate_handoff_owned(
            &provided,
            actual.view,
            handoff("transferred"),
            source,
            &[participant],
            source_cancel.clone(),
            pending.clone(),
            created,
            Arc::new(Effects::default()),
            &mut || Ok(()),
        )
        .unwrap();
        source_cancel.cancel();
        source_renderer.revoke();
        pending.cancel();
        participant_renderer.revoke();
        assert!(
            take_generate_handoff_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &actual,
                &consumer_cancel
            )
            .unwrap()
            .is_some()
        );
        let result = report_generate_run_started_owned(
            &provided,
            GenerateHandoffTarget::Mindmap,
            &actual,
            &consumer_cancel,
            &["/source.md".into()],
            2.0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.handoff_id, "transferred");
    }

    #[test]
    fn matching_initial_renderer_takes_once_and_records_original_materials_once() {
        let provided = Provided::default();
        let source = owner(ViewId(1), RendererGeneration::new());
        let created = RendererGeneration::new();
        let actual = owner(ViewId(2), created.clone());
        let cancel = CancelToken::default();
        queue_generate_handoff_owned(
            &provided,
            actual.view,
            handoff("valid"),
            source,
            &[],
            cancel.clone(),
            CancelToken::default(),
            created,
            Arc::new(Effects::default()),
            &mut || Ok(()),
        )
        .unwrap();
        assert!(
            take_generate_handoff_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &actual,
                &cancel
            )
            .unwrap()
            .is_some()
        );
        let result = report_generate_run_started_owned(
            &provided,
            GenerateHandoffTarget::Mindmap,
            &actual,
            &cancel,
            &["/source.md".into()],
            2.0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.handoff_id, "valid");
        assert_eq!(result.sources, vec!["/source.md"]);
        assert!(
            report_generate_run_started_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &actual,
                &cancel,
                &["/source.md".into()],
                3.0
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(
            super::super::shared_handoff_store(&provided)
                .borrow()
                .generate_run_started_events()
                .len(),
            1
        );
    }

    #[test]
    fn source_cancellation_before_transfer_publishes_no_handoff() {
        let provided = Provided::default();
        let source = owner(ViewId(1), RendererGeneration::new());
        let cancel = CancelToken::default();
        cancel.cancel();
        assert!(
            queue_generate_handoff_owned(
                &provided,
                ViewId(2),
                handoff("denied"),
                source,
                &[],
                cancel,
                CancelToken::default(),
                RendererGeneration::new(),
                Arc::new(Effects::default()),
                &mut || Ok(())
            )
            .is_err()
        );
        assert!(
            super::super::shared_handoff_store(&provided)
                .borrow()
                .queued_generate_handoffs()
                .is_empty()
        );
        assert!(owners(&provided).borrow().0.is_empty());
    }

    #[test]
    fn replacement_after_transfer_prevents_both_take_and_result() {
        let provided = Provided::default();
        let source = owner(ViewId(1), RendererGeneration::new());
        let created = RendererGeneration::new();
        let actual = owner(ViewId(2), created.clone());
        let cancel = CancelToken::default();
        queue_generate_handoff_owned(
            &provided,
            actual.view,
            handoff("replaced"),
            source,
            &[],
            cancel.clone(),
            CancelToken::default(),
            created.clone(),
            Arc::new(Effects::default()),
            &mut || Ok(()),
        )
        .unwrap();
        assert!(
            take_generate_handoff_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &actual,
                &cancel
            )
            .unwrap()
            .is_some()
        );
        created.revoke();
        assert!(
            take_generate_handoff_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &actual,
                &cancel
            )
            .is_err()
        );
        assert!(
            report_generate_run_started_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &actual,
                &cancel,
                &["/source.md".into()],
                2.0
            )
            .is_err()
        );
        assert!(
            super::super::shared_handoff_store(&provided)
                .borrow()
                .generate_run_started_events()
                .is_empty()
        );
    }

    #[test]
    fn source_replacement_before_transfer_publishes_no_handoff() {
        let provided = Provided::default();
        let generation = RendererGeneration::new();
        let source = owner(ViewId(1), generation.clone());
        generation.revoke();
        assert!(
            queue_generate_handoff_owned(
                &provided,
                ViewId(2),
                handoff("denied"),
                source,
                &[],
                CancelToken::default(),
                CancelToken::default(),
                RendererGeneration::new(),
                Arc::new(Effects::default()),
                &mut || Ok(())
            )
            .is_err()
        );
        assert!(
            super::super::shared_handoff_store(&provided)
                .borrow()
                .queued_generate_handoffs()
                .is_empty()
        );
    }

    #[test]
    fn initializer_failure_does_not_publish_initial_payload() {
        let provided = Provided::default();
        let source = owner(ViewId(1), RendererGeneration::new());
        let error = queue_generate_handoff_owned(
            &provided,
            ViewId(2),
            handoff("not-published"),
            source,
            &[],
            CancelToken::default(),
            CancelToken::default(),
            RendererGeneration::new(),
            Arc::new(Effects::default()),
            &mut || Err("placement rejected".into()),
        )
        .err()
        .unwrap();
        assert_eq!(error, "placement rejected");
        assert!(
            super::super::shared_handoff_store(&provided)
                .borrow()
                .queued_generate_handoffs()
                .is_empty()
        );
        assert!(owners(&provided).borrow().0.is_empty());
    }
    #[test]
    fn pending_or_participant_loss_prevents_publication_and_initializer() {
        for pending_cancelled in [true, false] {
            let provided = Provided::default();
            let source = owner(ViewId(1), RendererGeneration::new());
            let participant_generation = RendererGeneration::new();
            let participant = owner(ViewId(3), participant_generation.clone());
            let pending = CancelToken::default();
            if pending_cancelled {
                pending.cancel()
            } else {
                participant_generation.revoke()
            }
            let mut initialized = false;
            let result = queue_generate_handoff_owned(
                &provided,
                ViewId(2),
                handoff("denied"),
                source,
                &[participant],
                CancelToken::default(),
                pending,
                RendererGeneration::new(),
                Arc::new(Effects::default()),
                &mut || {
                    initialized = true;
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert!(!initialized);
            assert!(
                super::super::shared_handoff_store(&provided)
                    .borrow()
                    .queued_generate_handoffs()
                    .is_empty()
            );
            assert!(owners(&provided).borrow().0.is_empty());
        }
    }
    #[test]
    fn rollback_restores_displaced_entry_and_rejects_equal_replacement() {
        let provided = Provided::default();
        let view = ViewId(2);
        let created = RendererGeneration::new();
        let source = owner(ViewId(1), RendererGeneration::new());
        let pending = || CancelToken::default();
        let queue = |value| {
            queue_generate_handoff_owned(
                &provided,
                view,
                handoff(value),
                source.clone(),
                &[],
                pending(),
                pending(),
                created.clone(),
                Arc::new(Effects::default()),
                &mut || Ok(()),
            )
            .unwrap()
        };
        let old = queue("old");
        let next = queue("new");
        next.rollback().unwrap();
        next.rollback().unwrap();
        let current = owner(view, created.clone());
        assert_eq!(
            take_generate_handoff_owned(
                &provided,
                GenerateHandoffTarget::Mindmap,
                &current,
                &pending()
            )
            .unwrap()
            .unwrap()
            .id,
            "old"
        );
        old.rollback().unwrap();
        let first = queue("same");
        let second = queue("same");
        assert!(first.rollback().is_err());
        second.rollback().unwrap();
        first.rollback().unwrap();
    }
    #[test]
    fn only_original_reservation_allows_post_close_rollback() {
        let provided = Provided::default();
        let view = ViewId(2);
        let created = RendererGeneration::new();
        let source = owner(ViewId(1), RendererGeneration::new());
        let (issuer, reservation) =
            shell_contract::ipc::NativeClosureIssuer::new(view, created.clone());
        let receipt = super::queue_generate_handoff_owned(
            &provided,
            view,
            handoff("new"),
            source,
            &[],
            CancelToken::default(),
            CancelToken::default(),
            created.clone(),
            reservation,
            Arc::new(Effects::default()),
            &mut || Ok(()),
        )
        .unwrap();
        created.revoke();
        assert!(receipt.rollback().is_err());
        let (forged, _) = shell_contract::ipc::NativeClosureIssuer::new(view, created.clone());
        assert!(receipt.retire_closed(&forged.closed()).is_err());
        receipt.retire_closed(&issuer.closed()).unwrap();
        receipt.rollback().unwrap();
        assert!(
            super::super::shared_handoff_store(&provided)
                .borrow()
                .queued_generate_handoffs()
                .is_empty()
        );
    }
}
