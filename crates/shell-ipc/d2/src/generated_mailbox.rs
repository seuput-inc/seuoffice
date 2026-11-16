//! Retain the original generated-output completion across cleanup and core wakes.
//!
//! One implementation for every d2 actor that publishes a generated output: the
//! actor supplies its services, its mailbox and the shell's cleanup registration.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use crate::docs::hosts::GeneratedActorWake;
use crate::kit::{self, CoreReplay};
use shell_contract::hosts::{
    GeneratedObserveFailure, GeneratedPathObserver, GeneratedPathOutcome, GeneratedPathPending,
    GeneratedPathWake, GeneratedTerminalDelivery, PreparedGeneratedPath,
};
use shell_contract::runtime::Services;

/// What an actor gives the mailbox. The actor lives on the core thread.
pub(crate) trait Actor: 'static {
    fn services(&self) -> &Services;
    fn mailbox(&self) -> &CleanupMailbox;
    /// The shell's process-owned retry of this actor's retained completions.
    fn register_cleanup(
        &self,
        wake: GeneratedActorWake,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String>;
}

type Outcome = Result<GeneratedPathOutcome, String>;
pub(crate) struct Finished {
    complete: Box<dyn FnOnce()>,
    id: u64,
}
impl Finished {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn complete(self) {
        (self.complete)();
    }
}
type Consumer = Box<dyn FnOnce(Outcome, Finished)>;

fn finished<A: Actor>(owner: &Rc<A>, id: u64) -> Finished {
    let weak = Rc::downgrade(owner);
    Finished {
        complete: Box::new(move || {
            let _ = complete(&weak, id);
        }),
        id,
    }
}

enum Message {
    Terminal(GeneratedTerminalDelivery),
}
#[derive(Default)]
struct Inbox {
    message: Mutex<Option<Message>>,
    replay: Mutex<Option<CoreReplay>>,
}
impl Inbox {
    fn send(&self, message: Message) {
        let mut slot = self
            .message
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_some() {
            return;
        }
        *slot = Some(message);
        drop(slot);
        self.wake();
    }
    fn wake(&self) {
        if self
            .message
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none()
        {
            return;
        }
        let replay = self
            .replay
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(replay) = replay {
            replay.post(Box::new(|_| {}));
        }
    }
}

struct Entry {
    inbox: Arc<Inbox>,
    consumer: Option<Consumer>,
    receipt: Option<Arc<dyn GeneratedPathPending>>,
    observer: Option<(GeneratedPathObserver, GeneratedPathWake)>,
    prepared: Option<Box<dyn PreparedGeneratedPath>>,
    deliveries: Vec<Arc<Mutex<GeneratedTerminalDelivery>>>,
    settled: bool,
    acking: bool,
    registration: Option<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>>,
}

trait Wake: Send + Sync {
    fn wake(&self);
}
struct CallbackInbox<T> {
    value: Mutex<Option<T>>,
    replay: Mutex<Option<CoreReplay>>,
}
impl<T: Send> Wake for CallbackInbox<T> {
    fn wake(&self) {
        if self
            .value
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none()
        {
            return;
        }
        let replay = self
            .replay
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(replay) = replay {
            replay.post(Box::new(|_| {}));
        }
    }
}

#[derive(Default)]
pub(crate) struct CleanupMailbox {
    entries: RefCell<HashMap<u64, Entry>>,
    #[allow(clippy::type_complexity)]
    callbacks: RefCell<HashMap<u64, (u64, Arc<dyn Wake>)>>,
}

/// Preserve an off-core UI completion until its core continuation really runs.
pub(crate) fn callback<A: Actor, T: Send + 'static>(
    actor: &Rc<A>,
    owner: u64,
    then: impl FnOnce(T) + 'static,
) -> Box<dyn FnOnce(T) + Send> {
    let id = kit::unique();
    let inbox = Arc::new(CallbackInbox {
        value: Mutex::new(None),
        replay: Mutex::new(None),
    });
    let captured = inbox.clone();
    let weak = Rc::downgrade(actor);
    let replay = kit::replay_on_core(actor.services(), move || {
        let value = captured
            .value
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(actor) = weak.upgrade() {
            actor.mailbox().callbacks.borrow_mut().remove(&id);
        }
        if let Some(value) = value {
            then(value);
        }
    });
    *inbox
        .replay
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(replay);
    actor
        .mailbox()
        .callbacks
        .borrow_mut()
        .insert(id, (owner, inbox.clone()));
    Box::new(move |value| {
        *inbox
            .value
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(value);
        inbox.wake();
    })
}

fn arm<A: Actor>(actor: &Rc<A>, id: u64, inbox: &Arc<Inbox>) {
    let weak = Rc::downgrade(actor);
    let replay = kit::replay_on_core(actor.services(), move || {
        if let Some(actor) = weak.upgrade() {
            dispatch(&actor, id);
        }
    });
    *inbox
        .replay
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(replay);
}

#[derive(Default)]
struct RetryWake {
    replay: Mutex<Option<CoreReplay>>,
    result: Mutex<Option<Result<(), String>>>,
}
fn arm_retry<A: Actor>(actor: &Rc<A>, id: u64, retry: &Arc<RetryWake>) {
    let weak = Rc::downgrade(actor);
    let again = retry.clone();
    let replay = kit::replay_on_core(actor.services(), move || {
        let result = match weak.upgrade() {
            Some(actor) => {
                let result = wake_one(&actor, id);
                if actor.mailbox().entries.borrow().contains_key(&id) {
                    arm_retry(&actor, id, &again);
                }
                result
            }
            None => Err("The generated-output actor is unavailable".into()),
        };
        *again
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result);
    });
    *retry
        .replay
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(replay);
}

/// Install before the host can synchronously or asynchronously complete.
pub(crate) fn begin<A: Actor>(actor: &Rc<A>, consumer: Consumer) -> Option<Ticket> {
    wake(actor);
    let id = kit::unique();
    let inbox = Arc::new(Inbox::default());
    let retry = Arc::new(RetryWake::default());
    arm(actor, id, &inbox);
    arm_retry(actor, id, &retry);
    actor.mailbox().entries.borrow_mut().insert(
        id,
        Entry {
            inbox: inbox.clone(),
            consumer: Some(consumer),
            receipt: None,
            observer: None,
            prepared: None,
            deliveries: Vec::new(),
            settled: false,
            acking: false,
            registration: None,
        },
    );
    let retry_signal = retry;
    let actor_wake: GeneratedActorWake = Arc::new(move |done| {
        let replay = retry_signal
            .replay
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        match replay {
            Some(replay) => {
                let result = retry_signal.clone();
                replay.post(Box::new(move |posted| {
                    done(posted.and_then(|()| {
                        result
                            .result
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take()
                            .unwrap_or_else(|| {
                                Err("The cleanup retry did not report its result".into())
                            })
                    }));
                }));
            }
            None => done(Err("The cleanup continuation is unavailable".into())),
        }
    });
    match actor.register_cleanup(actor_wake) {
        Ok(registration) => {
            if let Some(entry) = actor.mailbox().entries.borrow_mut().get_mut(&id) {
                entry.registration = Some(registration);
            }
        }
        Err(error) => {
            let consumer = actor
                .mailbox()
                .entries
                .borrow_mut()
                .get_mut(&id)
                .and_then(|entry| entry.consumer.take());
            if let Some(consumer) = consumer {
                consumer(Err(error), finished(actor, id));
            }
            return None;
        }
    }
    let notify = inbox.clone();
    let wake: GeneratedPathWake = Arc::new(move || notify.wake());
    Some(Ticket { id, wake })
}

pub(crate) struct Ticket {
    id: u64,
    wake: GeneratedPathWake,
}
impl Ticket {
    pub(crate) fn wake(&self) -> GeneratedPathWake {
        self.wake.clone()
    }
    #[cfg(test)]
    pub(crate) fn deliver<A: Actor>(self, actor: &Rc<A>, delivery: GeneratedTerminalDelivery) {
        let inbox = actor
            .mailbox()
            .entries
            .borrow()
            .get(&self.id)
            .map(|entry| entry.inbox.clone());
        if let Some(inbox) = inbox {
            inbox.send(Message::Terminal(delivery));
        }
    }
    pub(crate) fn fail<A: Actor>(self, actor: &Rc<A>, error: String) {
        let consumer = actor
            .mailbox()
            .entries
            .borrow_mut()
            .get_mut(&self.id)
            .and_then(|entry| entry.consumer.take());
        if let Some(consumer) = consumer {
            consumer(Err(error), finished(actor, self.id));
        }
    }
    pub(crate) fn attach<A: Actor>(self, actor: &Rc<A>, prepared: Box<dyn PreparedGeneratedPath>) {
        let task = prepared.task();
        let inbox = {
            let mut entries = actor.mailbox().entries.borrow_mut();
            let Some(entry) = entries.get_mut(&self.id) else {
                return;
            };
            entry.receipt = Some(task.clone());
            entry.prepared = Some(prepared);
            entry.inbox.clone()
        };
        let terminal = inbox.clone();
        let observer: GeneratedPathObserver =
            Box::new(move |delivery| terminal.send(Message::Terminal(delivery)));
        match task.observe(observer, self.wake) {
            Ok(()) => {
                let _ = start_prepared(actor, self.id);
            }
            Err(GeneratedObserveFailure { observer, wake, .. }) => {
                if let Some(entry) = actor.mailbox().entries.borrow_mut().get_mut(&self.id) {
                    entry.observer = Some((observer, wake));
                }
            }
        }
    }
}

fn start_prepared<A: Actor>(actor: &Rc<A>, id: u64) -> Result<(), String> {
    let prepared = actor
        .mailbox()
        .entries
        .borrow_mut()
        .get_mut(&id)
        .and_then(|entry| entry.prepared.take());
    if let Some(prepared) = prepared
        && let Err(failure) = prepared.start()
    {
        let error = failure.error;
        if let Some(entry) = actor.mailbox().entries.borrow_mut().get_mut(&id) {
            entry.prepared = Some(failure.prepared);
        }
        return Err(error);
    }
    Ok(())
}

fn dispatch<A: Actor>(actor: &Rc<A>, id: u64) {
    let inbox = match actor.mailbox().entries.borrow().get(&id) {
        Some(entry) => entry.inbox.clone(),
        None => return,
    };
    let message = inbox
        .message
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    let Some(message) = message else {
        return;
    };
    let Message::Terminal(delivery) = message;
    let outcome = match delivery.result() {
        Ok(GeneratedPathOutcome::Opened(receipt)) => {
            Ok(GeneratedPathOutcome::Opened(receipt.clone()))
        }
        Ok(GeneratedPathOutcome::NotOpened) => Ok(GeneratedPathOutcome::NotOpened),
        Ok(GeneratedPathOutcome::CleanupPending(receipt)) => {
            Ok(GeneratedPathOutcome::CleanupPending(receipt.clone()))
        }
        Err(error) => Err(error.clone()),
    };
    if let Some(entry) = actor.mailbox().entries.borrow_mut().get_mut(&id) {
        entry.deliveries.push(Arc::new(Mutex::new(delivery)));
    }
    let outcome = match outcome {
        Ok(GeneratedPathOutcome::CleanupPending(receipt)) => {
            arm(actor, id, &inbox);
            let notify = inbox.clone();
            let wake: GeneratedPathWake = Arc::new(move || notify.wake());
            let terminal = inbox.clone();
            let observer: GeneratedPathObserver =
                Box::new(move |delivery| terminal.send(Message::Terminal(delivery)));
            if let Some(entry) = actor.mailbox().entries.borrow_mut().get_mut(&id) {
                entry.receipt = Some(receipt.clone());
            }
            if let Err(GeneratedObserveFailure { observer, wake, .. }) =
                receipt.observe(observer, wake)
                && let Some(entry) = actor.mailbox().entries.borrow_mut().get_mut(&id)
            {
                entry.observer = Some((observer, wake));
            }
            return;
        }
        outcome => outcome,
    };
    let consumer = actor
        .mailbox()
        .entries
        .borrow_mut()
        .get_mut(&id)
        .and_then(|entry| entry.consumer.take());
    if let Some(consumer) = consumer {
        consumer(outcome, finished(actor, id));
    }
}

fn complete<A: Actor>(weak: &Weak<A>, id: u64) -> Result<(), String> {
    let actor = weak
        .upgrade()
        .ok_or("The generated-output actor is unavailable")?;
    {
        let mut entries = actor.mailbox().entries.borrow_mut();
        let Some(entry) = entries.get_mut(&id) else {
            return Ok(());
        };
        entry.settled = true;
    }
    loop {
        let ack = {
            let mut entries = actor.mailbox().entries.borrow_mut();
            let Some(entry) = entries.get_mut(&id) else {
                return Ok(());
            };
            if entry.acking {
                return Err("The cleanup acknowledgement is already in progress".into());
            }
            match entry.deliveries.last().cloned() {
                Some(delivery) => {
                    entry.acking = true;
                    delivery
                }
                None => {
                    let mut entry = entries.remove(&id);
                    drop(entries);
                    if let Some(registration) =
                        entry.as_mut().and_then(|entry| entry.registration.take())
                    {
                        registration.complete();
                    }
                    return Ok(());
                }
            }
        };
        let result = ack
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .acknowledge();
        if let Some(entry) = actor.mailbox().entries.borrow_mut().get_mut(&id) {
            entry.acking = false;
            if result.is_ok() {
                entry.deliveries.pop();
            }
        }
        result?;
    }
}

/// Retry the same retained observer, wake or terminal acknowledgement; never
/// installs a second observer after one was accepted.
pub(crate) fn wake<A: Actor>(actor: &Rc<A>) {
    let ids: Vec<_> = actor.mailbox().entries.borrow().keys().copied().collect();
    for id in ids {
        let _ = wake_one(actor, id);
    }
}

fn wake_one<A: Actor>(actor: &Rc<A>, id: u64) -> Result<(), String> {
    let callbacks: Vec<_> = actor
        .mailbox()
        .callbacks
        .borrow()
        .values()
        .filter(|(owner, _)| *owner == id)
        .map(|(_, wake)| wake.clone())
        .collect();
    for callback in callbacks {
        callback.wake();
    }
    let (receipt, observer, inbox, settled) = {
        let mut entries = actor.mailbox().entries.borrow_mut();
        let Some(entry) = entries.get_mut(&id) else {
            return Ok(());
        };
        (
            entry.receipt.clone(),
            entry.observer.take(),
            entry.inbox.clone(),
            entry.settled,
        )
    };
    if settled {
        return complete(&Rc::downgrade(actor), id);
    }
    if let (Some(receipt), Some((observer, wake))) = (receipt.as_ref(), observer) {
        match receipt.observe(observer, wake) {
            Ok(()) => start_prepared(actor, id)?,
            Err(failure) => {
                let error = failure.error;
                if let Some(entry) = actor.mailbox().entries.borrow_mut().get_mut(&id) {
                    entry.observer = Some((failure.observer, failure.wake));
                }
                return Err(error);
            }
        }
    } else if let Some(receipt) = receipt {
        start_prepared(actor, id)?;
        match receipt.take_redelivery()? {
            Some(delivery) => inbox.send(Message::Terminal(delivery)),
            None => receipt.wake_observer()?,
        }
    }
    inbox.wake();
    Ok(())
}
