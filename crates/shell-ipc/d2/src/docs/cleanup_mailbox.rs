//! Retain the original generated-output completion across cleanup and core wakes.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use super::Docs;
use crate::kit::{self, CoreReplay};
use shell_contract::hosts::{
    GeneratedObserveFailure, GeneratedPathObserver, GeneratedPathOutcome, GeneratedPathPending,
    GeneratedPathWake, GeneratedTerminalDelivery, PreparedGeneratedPath,
};

type Outcome = Result<GeneratedPathOutcome, String>;
pub(super) struct Finished {
    docs: Weak<Docs>,
    id: u64,
}
impl Finished {
    pub(super) fn id(&self) -> u64 {
        self.id
    }
    pub(super) fn complete(self) {
        let _ = complete(&self.docs, self.id);
    }
}
type Consumer = Box<dyn FnOnce(Outcome, Finished)>;

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
    retry: Arc<RetryWake>,
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
pub(super) struct CleanupMailbox {
    entries: RefCell<HashMap<u64, Entry>>,
    callbacks: RefCell<HashMap<u64, (u64, Arc<dyn Wake>)>>,
}

/// Preserve an off-core UI completion until its core continuation really runs.
pub(super) fn callback<T: Send + 'static>(
    docs: &Rc<Docs>,
    owner: u64,
    then: impl FnOnce(T) + 'static,
) -> Box<dyn FnOnce(T) + Send> {
    let id = kit::unique();
    let inbox = Arc::new(CallbackInbox {
        value: Mutex::new(None),
        replay: Mutex::new(None),
    });
    let captured = inbox.clone();
    let weak = Rc::downgrade(docs);
    let replay = kit::replay_on_core(&docs.services, move || {
        let value = captured
            .value
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(docs) = weak.upgrade() {
            docs.cleanup_mailbox.callbacks.borrow_mut().remove(&id);
        }
        if let Some(value) = value {
            then(value);
        }
    });
    *inbox
        .replay
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(replay);
    docs.cleanup_mailbox
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

fn arm(docs: &Rc<Docs>, id: u64, inbox: &Arc<Inbox>) {
    let weak = Rc::downgrade(docs);
    let replay = kit::replay_on_core(&docs.services, move || {
        if let Some(docs) = weak.upgrade() {
            dispatch(&docs, id);
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
fn arm_retry(docs: &Rc<Docs>, id: u64, retry: &Arc<RetryWake>) {
    let weak = Rc::downgrade(docs);
    let again = retry.clone();
    let replay = kit::replay_on_core(&docs.services, move || {
        let result = match weak.upgrade() {
            Some(docs) => {
                let result = wake_one(&docs, id);
                if docs.cleanup_mailbox.entries.borrow().contains_key(&id) {
                    arm_retry(&docs, id, &again);
                }
                result
            }
            None => Err("The Docs cleanup actor is unavailable".into()),
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
pub(super) fn begin(docs: &Rc<Docs>, consumer: Consumer) -> Option<Ticket> {
    wake(docs);
    let id = kit::unique();
    let inbox = Arc::new(Inbox::default());
    let retry = Arc::new(RetryWake::default());
    arm(docs, id, &inbox);
    arm_retry(docs, id, &retry);
    docs.cleanup_mailbox.entries.borrow_mut().insert(
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
            retry: retry.clone(),
        },
    );
    let retry_signal = retry.clone();
    let actor_wake: super::hosts::GeneratedActorWake = Arc::new(move |done| {
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
                                Err("The Docs cleanup retry did not report its result".into())
                            })
                    }));
                }));
            }
            None => done(Err("The Docs cleanup continuation is unavailable".into())),
        }
    });
    match docs.shell.register_generated_actor_cleanup(actor_wake) {
        Ok(registration) => {
            if let Some(entry) = docs.cleanup_mailbox.entries.borrow_mut().get_mut(&id) {
                entry.registration = Some(registration);
            }
        }
        Err(error) => {
            let consumer = docs
                .cleanup_mailbox
                .entries
                .borrow_mut()
                .get_mut(&id)
                .and_then(|entry| entry.consumer.take());
            if let Some(consumer) = consumer {
                consumer(
                    Err(error),
                    Finished {
                        docs: Rc::downgrade(docs),
                        id,
                    },
                );
            }
            return None;
        }
    }
    let notify = inbox.clone();
    let wake: GeneratedPathWake = Arc::new(move || notify.wake());
    Some(Ticket { id, wake })
}

pub(super) struct Ticket {
    id: u64,
    wake: GeneratedPathWake,
}
impl Ticket {
    pub(super) fn wake(&self) -> GeneratedPathWake {
        self.wake.clone()
    }
    #[cfg(test)]
    pub(super) fn deliver(self, docs: &Rc<Docs>, delivery: GeneratedTerminalDelivery) {
        let inbox = docs
            .cleanup_mailbox
            .entries
            .borrow()
            .get(&self.id)
            .map(|entry| entry.inbox.clone());
        if let Some(inbox) = inbox {
            inbox.send(Message::Terminal(delivery));
        }
    }
    pub(super) fn fail(self, docs: &Rc<Docs>, error: String) {
        let consumer = docs
            .cleanup_mailbox
            .entries
            .borrow_mut()
            .get_mut(&self.id)
            .and_then(|entry| entry.consumer.take());
        if let Some(consumer) = consumer {
            consumer(
                Err(error),
                Finished {
                    docs: Rc::downgrade(docs),
                    id: self.id,
                },
            );
        }
    }
    pub(super) fn attach(self, docs: &Rc<Docs>, prepared: Box<dyn PreparedGeneratedPath>) {
        let task = prepared.task();
        let inbox = {
            let mut entries = docs.cleanup_mailbox.entries.borrow_mut();
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
                let _ = start_prepared(docs, self.id);
            }
            Err(GeneratedObserveFailure { observer, wake, .. }) => {
                if let Some(entry) = docs.cleanup_mailbox.entries.borrow_mut().get_mut(&self.id) {
                    entry.observer = Some((observer, wake));
                }
            }
        }
    }
}

fn start_prepared(docs: &Rc<Docs>, id: u64) -> Result<(), String> {
    let prepared = docs
        .cleanup_mailbox
        .entries
        .borrow_mut()
        .get_mut(&id)
        .and_then(|entry| entry.prepared.take());
    if let Some(prepared) = prepared {
        if let Err(failure) = prepared.start() {
            let error = failure.error;
            if let Some(entry) = docs.cleanup_mailbox.entries.borrow_mut().get_mut(&id) {
                entry.prepared = Some(failure.prepared);
            }
            return Err(error);
        }
    }
    Ok(())
}

fn dispatch(docs: &Rc<Docs>, id: u64) {
    let inbox = match docs.cleanup_mailbox.entries.borrow().get(&id) {
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
    if let Some(entry) = docs.cleanup_mailbox.entries.borrow_mut().get_mut(&id) {
        entry.deliveries.push(Arc::new(Mutex::new(delivery)));
    }
    let outcome = match outcome {
        Ok(GeneratedPathOutcome::CleanupPending(receipt)) => {
            arm(docs, id, &inbox);
            let notify = inbox.clone();
            let wake: GeneratedPathWake = Arc::new(move || notify.wake());
            let terminal = inbox.clone();
            let observer: GeneratedPathObserver =
                Box::new(move |delivery| terminal.send(Message::Terminal(delivery)));
            if let Some(entry) = docs.cleanup_mailbox.entries.borrow_mut().get_mut(&id) {
                entry.receipt = Some(receipt.clone());
            }
            if let Err(GeneratedObserveFailure { observer, wake, .. }) =
                receipt.observe(observer, wake)
            {
                if let Some(entry) = docs.cleanup_mailbox.entries.borrow_mut().get_mut(&id) {
                    entry.observer = Some((observer, wake));
                }
            }
            return;
        }
        outcome => outcome,
    };
    let consumer = docs
        .cleanup_mailbox
        .entries
        .borrow_mut()
        .get_mut(&id)
        .and_then(|entry| entry.consumer.take());
    if let Some(consumer) = consumer {
        consumer(
            outcome,
            Finished {
                docs: Rc::downgrade(docs),
                id,
            },
        );
    }
}

fn complete(weak: &Weak<Docs>, id: u64) -> Result<(), String> {
    let docs = weak
        .upgrade()
        .ok_or("The Docs cleanup actor is unavailable")?;
    {
        let mut entries = docs.cleanup_mailbox.entries.borrow_mut();
        let Some(entry) = entries.get_mut(&id) else {
            return Ok(());
        };
        entry.settled = true;
    }
    loop {
        let ack = {
            let mut entries = docs.cleanup_mailbox.entries.borrow_mut();
            let Some(entry) = entries.get_mut(&id) else {
                return Ok(());
            };
            if entry.acking {
                return Err("The Docs cleanup acknowledgement is already in progress".into());
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
        if let Some(entry) = docs.cleanup_mailbox.entries.borrow_mut().get_mut(&id) {
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
pub(super) fn wake(docs: &Rc<Docs>) {
    let ids: Vec<_> = docs
        .cleanup_mailbox
        .entries
        .borrow()
        .keys()
        .copied()
        .collect();
    for id in ids {
        let _ = wake_one(docs, id);
    }
}

fn wake_one(docs: &Rc<Docs>, id: u64) -> Result<(), String> {
    let callbacks: Vec<_> = docs
        .cleanup_mailbox
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
        let mut entries = docs.cleanup_mailbox.entries.borrow_mut();
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
        return complete(&Rc::downgrade(docs), id);
    }
    if let (Some(receipt), Some((observer, wake))) = (receipt.as_ref(), observer) {
        match receipt.observe(observer, wake) {
            Ok(()) => start_prepared(docs, id)?,
            Err(failure) => {
                let error = failure.error;
                if let Some(entry) = docs.cleanup_mailbox.entries.borrow_mut().get_mut(&id) {
                    entry.observer = Some((failure.observer, failure.wake));
                }
                return Err(error);
            }
        }
    } else if let Some(receipt) = receipt {
        start_prepared(docs, id)?;
        match receipt.take_redelivery()? {
            Some(delivery) => inbox.send(Message::Terminal(delivery)),
            None => receipt.wake_observer()?,
        }
    }
    inbox.wake();
    Ok(())
}
