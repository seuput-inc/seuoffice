//! Recovery data captured from the original identity's worker FIFO.

use std::sync::Arc;

use super::super::atomic_write::owned::{Admission, admitted};
use super::{
    RecoveryCopyInfo, RecoveryIdentity, RecoveryPinnedRead, RecoveryPinnedSnapshot, pair_queue,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedPairKind {
    Published,
    LegacyObserved,
}

/// This is data evidence, not permission to perform a document or filesystem effect.
#[derive(Clone)]
pub struct CapturedPair {
    kind: CapturedPairKind,
    snapshot: Arc<RecoveryPinnedSnapshot>,
    queue: Arc<pair_queue::RecoveryPairQueue>,
}

impl CapturedPair {
    pub fn kind(&self) -> CapturedPairKind {
        self.kind
    }
    pub fn bytes(&self) -> &[u8] {
        &self.snapshot.bytes
    }
    pub fn info(&self, original_mtime: Option<f64>) -> Option<RecoveryCopyInfo> {
        self.snapshot.copy_info(original_mtime)
    }
    pub fn snapshot(&self) -> &Arc<RecoveryPinnedSnapshot> {
        &self.snapshot
    }
    pub fn queue(&self) -> &Arc<pair_queue::RecoveryPairQueue> {
        &self.queue
    }
}

type CaptureDone = Box<dyn FnOnce(Result<Option<CapturedPair>, String>) + Send>;

type CaptureInput = (
    String,
    RecoveryIdentity,
    Admission,
    CaptureDone,
    Arc<pair_queue::RecoveryPairQueue>,
);

/// The callback runs on the identity worker. `admit` is the original saved
/// document authority, not the captured pair; no later view lookup is made.
pub fn capture_saved_pair(
    root: String,
    identity: RecoveryIdentity,
    admit: Admission,
    done: CaptureDone,
) {
    let queue = match pair_queue::for_identity(&root, &identity) {
        Ok(queue) => queue,
        Err(error) => return done(Err(error.message)),
    };
    let captured_queue = queue.clone();
    let input = (root, identity, admit, done, captured_queue);
    let job = |(root, identity, admit, done, queue): CaptureInput,
               turn: pair_queue::RecoveryPairTurn| {
        let result = (|| {
            turn.require_clean().map_err(|error| error.message)?;
            admitted(&admit, || ()).map_err(|error| error.message)?;
            let read = RecoveryPinnedRead::open_saved(&root, &identity);
            let read = match read {
                Ok(read) => read,
                Err(error) if error.code == Some("ENOENT") => {
                    admitted(&admit, || ()).map_err(|error| error.message)?;
                    return Ok(None);
                }
                Err(error) => return Err(error.message),
            };
            let snapshot = match read.read() {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    admitted(&admit, || ()).map_err(|error| error.message)?;
                    return Ok(None);
                }
            };
            admitted(&admit, || ()).map_err(|error| error.message)?;
            Ok(Some(CapturedPair {
                kind: CapturedPairKind::LegacyObserved,
                snapshot: Arc::new(snapshot),
                queue,
            }))
        })();
        done(result);
        drop(turn);
    };
    if let Err((error, (_, _, _, done, _), _job)) = queue.submit_owned(input, job) {
        done(Err(error.message));
    }
}
