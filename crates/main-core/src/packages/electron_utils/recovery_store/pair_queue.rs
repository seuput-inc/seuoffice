//! Worker-only FIFO for the two files belonging to one recovery identity.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, atomic::{AtomicU64, Ordering}};

use super::{RecoveryIdentity, queue_key, recovery_copy_path};
use crate::node_fs::NodeError;

type Action = Box<dyn FnOnce(RecoveryPairTurn) + Send + 'static>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnKind { Operation, Delete }

struct Job { kind: TurnKind, action: Action }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecoveryPairPhase {
    #[default]
    Clean,
    WriteIntent,
    MetadataPublished,
    CopyPublished,
    DeleteIntent,
    CopyRemoved,
    MetadataRemoved,
}

#[derive(Default)]
struct State {
    phase: RecoveryPairPhase,
    running: bool,
    jobs: VecDeque<Job>,
}

pub struct RecoveryPairQueue {
    key: String,
    state: Mutex<State>,
    epoch: AtomicU64,
}

/// The queue admits the next operation only after this turn is dropped.
pub struct RecoveryPairTurn {
    queue: Arc<RecoveryPairQueue>,
    kind: TurnKind,
}

#[derive(Clone)]
pub struct RecoveryPairTicket {
    queue: Arc<RecoveryPairQueue>,
    epoch: u64,
}

static QUEUES: LazyLock<Mutex<HashMap<String, Arc<RecoveryPairQueue>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn for_identity(root: &str, identity: &RecoveryIdentity) -> Result<Arc<RecoveryPairQueue>, NodeError> {
    for_copy_path(&recovery_copy_path(root, identity)?)
}

pub(super) fn for_copy_path(copy: &str) -> Result<Arc<RecoveryPairQueue>, NodeError> {
    let path = queue_key(copy);
    let mut queues = QUEUES.lock().map_err(|_| error("recovery queues are unavailable"))?;
    Ok(queues.entry(path.clone()).or_insert_with(|| Arc::new(RecoveryPairQueue {
        key: path, state: Mutex::new(State::default()), epoch: AtomicU64::new(0),
    })).clone())
}

impl RecoveryPairTicket {
    pub(super) fn belongs_to(&self, queue: &Arc<RecoveryPairQueue>) -> bool {
        Arc::ptr_eq(&self.queue, queue)
    }
}

fn error(message: &str) -> NodeError {
    NodeError { code: None, message: message.to_owned() }
}

impl RecoveryPairQueue {
    /// Capture before serialization. A subsequent delete invalidates this ticket.
    pub fn ticket(self: &Arc<Self>) -> Result<RecoveryPairTicket, NodeError> {
        Ok(RecoveryPairTicket { queue: self.clone(), epoch: self.epoch.load(Ordering::Acquire) & !1 })
    }

    /// Invalidate prepared writes and enqueue the delete in the same queue turn.
    pub fn submit_delete<F>(self: &Arc<Self>, job: F) -> Result<(), (NodeError, F)>
    where F: FnOnce(RecoveryPairTurn) + Send + 'static {
        let start = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return Err((error("recovery queue is unavailable"), job)),
            };
            if self.epoch.fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| epoch.checked_add(2)).is_err() {
                return Err((error("recovery epoch exhausted"), job));
            }
            state.jobs.push_back(Job { kind: TurnKind::Delete, action: Box::new(job) });
            if state.running { false } else { state.running = true; true }
        };
        if start { self.dispatch(); }
        Ok(())
    }

    pub fn submit_delete_owned<T, F>(self: &Arc<Self>, input: T, job: F) -> Result<(), (NodeError, T, F)>
    where T: Send + 'static, F: FnOnce(T, RecoveryPairTurn) + Send + 'static {
        let start = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return Err((error("recovery queue is unavailable"), input, job)),
            };
            if self.epoch.fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| epoch.checked_add(2)).is_err() {
                return Err((error("recovery epoch exhausted"), input, job));
            }
            state.jobs.push_back(Job { kind: TurnKind::Delete, action: Box::new(move |turn| job(input, turn)) });
            if state.running { false } else { state.running = true; true }
        };
        if start { self.dispatch(); }
        Ok(())
    }

    /// The closure runs on a worker, never on the submitting or core thread.
    pub fn submit<F>(self: &Arc<Self>, job: F) -> Result<(), (NodeError, F)>
    where F: FnOnce(RecoveryPairTurn) + Send + 'static {
        let start = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return Err((error("recovery queue is unavailable"), job)),
            };
            state.jobs.push_back(Job { kind: TurnKind::Operation, action: Box::new(job) });
            if state.running { false } else { state.running = true; true }
        };
        if start { self.dispatch(); }
        Ok(())
    }

    /// Return the immutable input and callback intact if enqueueing fails.
    pub fn submit_owned<T, F>(self: &Arc<Self>, input: T, job: F) -> Result<(), (NodeError, T, F)>
    where T: Send + 'static, F: FnOnce(T, RecoveryPairTurn) + Send + 'static {
        let start = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return Err((error("recovery queue is unavailable"), input, job)),
            };
            state.jobs.push_back(Job { kind: TurnKind::Operation, action: Box::new(move |turn| job(input, turn)) });
            if state.running { false } else { state.running = true; true }
        };
        if start { self.dispatch(); }
        Ok(())
    }

    fn dispatch(self: &Arc<Self>) {
        let job = match self.state.lock() {
            Ok(mut state) => match state.jobs.pop_front() {
                Some(job) => Some(job),
                None => { state.running = false; None }
            },
            Err(_) => None,
        };
        if let Some(job) = job {
            let queue = self.clone();
            std::thread::spawn(move || (job.action)(RecoveryPairTurn { queue, kind: job.kind }));
        }
    }
}

impl RecoveryPairTurn {
    pub fn phase(&self) -> Result<RecoveryPairPhase, NodeError> {
        self.queue.state.lock().map(|state| state.phase).map_err(|_| error("recovery queue is unavailable"))
    }

    pub fn require_clean(&self) -> Result<(), NodeError> {
        let state = self.queue.state.lock().map_err(|_| error("recovery queue is unavailable"))?;
        if state.phase != RecoveryPairPhase::Clean { Err(error("recovery copy and metadata need repair")) } else { Ok(()) }
    }

    pub fn begin_mutation(&self) -> Result<(), NodeError> {
        let mut state = self.queue.state.lock().map_err(|_| error("recovery queue is unavailable"))?;
        if state.phase != RecoveryPairPhase::Clean { return Err(error("recovery pair requires repair")); }
        state.phase = match self.kind {
            TurnKind::Operation => RecoveryPairPhase::WriteIntent,
            TurnKind::Delete => RecoveryPairPhase::DeleteIntent,
        };
        Ok(())
    }

    pub fn advance(&self, from: RecoveryPairPhase, to: RecoveryPairPhase) -> Result<(), NodeError> {
        let valid = matches!((self.kind, from, to),
            (TurnKind::Operation, RecoveryPairPhase::WriteIntent, RecoveryPairPhase::MetadataPublished)
            | (TurnKind::Operation, RecoveryPairPhase::MetadataPublished, RecoveryPairPhase::CopyPublished)
            | (TurnKind::Delete, RecoveryPairPhase::DeleteIntent, RecoveryPairPhase::CopyRemoved)
            | (TurnKind::Delete, RecoveryPairPhase::CopyRemoved, RecoveryPairPhase::MetadataRemoved));
        if !valid { return Err(error("invalid recovery pair transition")); }
        let mut state = self.queue.state.lock().map_err(|_| error("recovery queue is unavailable"))?;
        if state.phase != from { return Err(error("recovery pair phase changed")); }
        state.phase = to;
        Ok(())
    }

    /// The delete turn may clear dirtiness only after both removals succeed.
    pub fn finish_delete(&self) -> Result<(), NodeError> {
        if self.kind != TurnKind::Delete { return Err(error("recovery turn is not a delete")); }
        let mut state = self.queue.state.lock().map_err(|_| error("recovery queue is unavailable"))?;
        if state.phase != RecoveryPairPhase::MetadataRemoved { return Err(error("recovery deletion is incomplete")); }
        state.phase = RecoveryPairPhase::Clean;
        Ok(())
    }

    /// The exact pair was superseded and all temporary moves were reversed.
    pub fn finish_superseded(&self) -> Result<(), NodeError> {
        if self.kind != TurnKind::Delete { return Err(error("recovery turn is not a delete")); }
        let mut state = self.queue.state.lock().map_err(|_| error("recovery queue is unavailable"))?;
        if state.phase != RecoveryPairPhase::DeleteIntent { return Err(error("recovery inverse is incomplete")); }
        state.phase = RecoveryPairPhase::Clean;
        Ok(())
    }

    /// A complete publication is clean only while its own ticket is current.
    pub fn finish_write(&self, ticket: &RecoveryPairTicket) -> Result<bool, NodeError> {
        self.with_current(ticket, || {
            let mut state = self.queue.state.lock().map_err(|_| error("recovery queue is unavailable"))?;
            if state.phase != RecoveryPairPhase::CopyPublished { return Err(error("recovery publication is incomplete")); }
            state.phase = RecoveryPairPhase::Clean;
            Ok(())
        })?.transpose().map(|result| result.is_some())
    }

    /// The CAS admits this one bounded effect before any later invalidation.
    /// The FIFO retains the turn until the effect and its continuation finish.
    pub fn with_current<T>(&self, ticket: &RecoveryPairTicket, effect: impl FnOnce() -> T) -> Result<Option<T>, NodeError> {
        if !Arc::ptr_eq(&self.queue, &ticket.queue) { return Err(error("recovery ticket belongs to another identity")); }
        match self.queue.epoch.compare_exchange(ticket.epoch, ticket.epoch | 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {}
            Err(current) if current & !1 != ticket.epoch => return Ok(None),
            Err(_) => return Err(error("a recovery namespace effect is already admitted")),
        }
        struct Effect<'a>(&'a AtomicU64);
        impl Drop for Effect<'_> {
            fn drop(&mut self) { self.0.fetch_and(!1, Ordering::Release); }
        }
        let _effect = Effect(&self.queue.epoch);
        Ok(Some(effect()))
    }
}

impl Drop for RecoveryPairTurn {
    fn drop(&mut self) { self.queue.dispatch(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn delete_invalidates_prepared_write_and_partial_pair_stays_dirty() {
        let queue = Arc::new(RecoveryPairQueue { key: String::new(), state: Mutex::new(State::default()), epoch: AtomicU64::new(0) });
        let ticket = queue.ticket().unwrap();
        let (started, wait_started) = mpsc::channel();
        let (resume, wait_resume) = mpsc::channel();
        let (finished, wait_finished) = mpsc::channel();
        queue.submit(move |turn| {
            started.send(()).unwrap();
            wait_resume.recv().unwrap();
            assert!(turn.with_current(&ticket, || ()).unwrap().is_none());
            turn.begin_mutation().unwrap();
            finished.send(()).unwrap();
        }).unwrap_or_else(|(error, _)| panic!("{error:?}"));
        wait_started.recv().unwrap();
        queue.submit_delete(|_| {}).unwrap_or_else(|(error, _)| panic!("{error:?}"));
        resume.send(()).unwrap();
        wait_finished.recv().unwrap();
        let (checked, wait_checked) = mpsc::channel();
        queue.submit(move |turn| {
            checked.send(turn.require_clean().is_err()).unwrap();
        }).unwrap_or_else(|(error, _)| panic!("{error:?}"));
        assert!(wait_checked.recv().unwrap());
    }

    #[test]
    fn admitted_effect_finishes_before_queued_delete() {
        let queue = Arc::new(RecoveryPairQueue { key: String::new(), state: Mutex::new(State::default()), epoch: AtomicU64::new(0) });
        let ticket = queue.ticket().unwrap();
        let (entered, wait_entered) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        let (order, observed) = mpsc::channel();
        let write_order = order.clone();
        queue.submit(move |turn| {
            turn.with_current(&ticket, || {
                entered.send(()).unwrap();
                wait_release.recv().unwrap();
                write_order.send("write").unwrap();
            }).unwrap().unwrap();
        }).unwrap_or_else(|(error, _)| panic!("{error:?}"));
        wait_entered.recv().unwrap();
        queue.submit_delete(move |_| { order.send("delete").unwrap(); }).unwrap_or_else(|(error, _)| panic!("{error:?}"));
        release.send(()).unwrap();
        assert_eq!(observed.recv().unwrap(), "write");
        assert_eq!(observed.recv().unwrap(), "delete");
    }

    #[test]
    fn partial_publication_cannot_be_acknowledged_as_clean() {
        let queue = Arc::new(RecoveryPairQueue { key: String::new(), state: Mutex::new(State::default()), epoch: AtomicU64::new(0) });
        let ticket = queue.ticket().unwrap();
        let (done, result) = mpsc::channel();
        queue.submit(move |turn| {
            turn.begin_mutation().unwrap();
            turn.advance(RecoveryPairPhase::WriteIntent, RecoveryPairPhase::MetadataPublished).unwrap();
            assert!(turn.finish_write(&ticket).is_err());
            assert!(turn.require_clean().is_err());
            done.send(()).unwrap();
        }).unwrap_or_else(|(error, _)| panic!("{error:?}"));
        result.recv().unwrap();
    }
}
