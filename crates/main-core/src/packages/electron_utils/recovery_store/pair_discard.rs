//! Conditional discard of the exact captured recovery pair.


use file_commit::PrivateDirectory;
use crate::node_fs::NodeError;
use super::super::atomic_write::owned::{Admission, admitted};
use super::captured_pair::CapturedPair;
use super::conditional_pair::{ConditionalPairCleanup, PairCleanupOutcome};
use super::pair_queue::{RecoveryPairPhase, RecoveryPairTurn};

pub type PairDiscardDone = Box<dyn FnOnce(Result<PairCleanupOutcome, PairDiscardFailure>) + Send>;

/// The caller must retain this value on failure; it owns the FIFO turn and
/// any quarantined entries or inverse operations until retry completes.
#[must_use]
pub struct PendingPairDiscard {
    pair: CapturedPair,
    admission: Admission,
    turn: RecoveryPairTurn,
    directory: Option<PrivateDirectory>,
    cleanup: Option<ConditionalPairCleanup>,
    mutation_started: bool,
}

pub enum PairDiscardFailure {
    NotQueued { error: NodeError, pair: CapturedPair, admission: Admission },
    Pending { error: NodeError, pending: PendingPairDiscard },
}

impl CapturedPair {
    /// `admission` must be the original document/choice authority or a
    /// restricted terminal cleanup capability; this pair itself grants none.
    pub fn discard(self, admission: Admission, done: PairDiscardDone) {
        let queue = self.queue().clone();
        let input = (self, admission, done);
        let job = |(pair, admission, done): (CapturedPair, Admission, PairDiscardDone), turn| {
            PendingPairDiscard { pair, admission, turn, directory: None, cleanup: None, mutation_started: false }.drive(done);
        };
        if let Err((error, (pair, admission, done), _job)) = queue.submit_delete_owned(input, job) {
            done(Err(PairDiscardFailure::NotQueued { error, pair, admission }));
        }
    }
}

impl PendingPairDiscard {
    /// Resumes on a worker while keeping the original identity FIFO turn.
    pub fn retry(self, done: PairDiscardDone) {
        std::thread::spawn(move || self.drive(done));
    }

    fn drive(mut self, done: PairDiscardDone) {
        if let Err(error) = self.prepare() {
            return done(Err(PairDiscardFailure::Pending { error, pending: self }));
        }
        loop {
            let cleanup = self.cleanup.take().expect("prepared cleanup is retained");
            match cleanup.step_owned(&self.admission) {
                Ok((cleanup, Some(outcome))) => {
                    self.cleanup = Some(cleanup);
                    let result = (|| {
                        match outcome {
                            PairCleanupOutcome::Deleted => {
                                self.turn.advance(RecoveryPairPhase::DeleteIntent, RecoveryPairPhase::CopyRemoved)?;
                                self.turn.advance(RecoveryPairPhase::CopyRemoved, RecoveryPairPhase::MetadataRemoved)?;
                                self.turn.finish_delete()?;
                            }
                            PairCleanupOutcome::Superseded => self.turn.finish_superseded()?,
                        }
                        Ok(outcome)
                    })();
                    return match result {
                        Ok(outcome) => done(Ok(outcome)),
                        Err(error) => done(Err(PairDiscardFailure::Pending { error, pending: self })),
                    };
                }
                Ok((cleanup, None)) => self.cleanup = Some(cleanup),
                Err((error, cleanup)) => {
                    self.cleanup = Some(cleanup);
                    return done(Err(PairDiscardFailure::Pending { error, pending: self }));
                }
            }
        }
    }

    fn prepare(&mut self) -> Result<(), NodeError> {
        if !self.mutation_started {
            self.turn.require_clean()?;
            admitted(&self.admission, || ())?;
            self.turn.begin_mutation()?;
            self.mutation_started = true;
        }
        if self.directory.is_none() {
            let parent = self.pair.snapshot().parent_handle().ok_or_else(|| NodeError { code: None, message: "the captured recovery directory is unavailable".into() })?;
            let directory = PrivateDirectory::create(parent).map_err(|error| NodeError { code: None, message: error.to_string() })?;
            self.directory = Some(directory);
            admitted(&self.admission, || ())?;
        }
        if self.cleanup.is_none() {
            self.cleanup = Some(ConditionalPairCleanup::new(self.pair.snapshot().clone(), self.directory.as_ref().expect("directory is retained"))?);
        }
        Ok(())
    }
}
