//! `packages/electron-utils/src/recovery-store.ts`: crash-recovery copies
//! shared by the editors and the shell.
//!
//! Layout under `<userData>`: `<kind>-autosave/<sha1(filePath)[0..16]><ext>`
//! (file copy), `<kind>-autosave/untitled/<id><ext>` (untitled copy), each
//! with a `.meta.json` beside it. See the TypeScript for the full rules.
//!
//! What differs from the TypeScript, and why it changes nothing a caller can
//! see:
//! - The process-wide state (`Symbol.for` on `globalThis`) is an explicit
//!   [`SharedRecoveryState`] the caller owns and passes to
//!   [`create_recovery_store`] and to the shutdown functions.
//! - Promises are plain calls. The per-document write queue of the
//!   TypeScript only orders asynchronous tasks; a sequence of plain calls is
//!   that order already. What interleaves in the TypeScript (a delete during
//!   the serialization of a streamed copy) is reached through the writer
//!   callback of [`RecoveryStore::write_copy_streamed`], which may call
//!   [`RecoveryStore::delete_copy`] on the same store, and through tickets
//!   taken earlier with [`RecoveryStore::begin_write`].
//! - `Date.now()`, `crypto.randomUUID()` and `userDataDir` come in through
//!   [`RecoveryStoreOptions`].

#[cfg(any(unix, windows))]
pub mod captured_pair;
#[cfg(any(unix, windows))]
pub mod conditional_entry;
#[cfg(any(unix, windows))]
pub mod conditional_pair;
#[cfg(any(unix, windows))]
pub mod pair_discard;
pub mod pair_journal;
pub mod pair_queue;
#[cfg(any(unix, windows))]
pub mod strict_marker;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;

use jscompat::node_path::host::{basename, dirname, extname, is_absolute, join, relative, resolve};
use jscompat::{JsObject, JsonValue, js_regex, json_parse, random_uuid, sha1_hex};

use super::atomic_write::{
    atomic_move_file, atomic_write_file, temp_path_beside, write_json_atomic,
};
use crate::node_fs::{NodeError, node_error};

/// The editors that keep recovery copies, in folder order.
pub const RECOVERY_EDITOR_KINDS: [&str; 9] = [
    "docs", "sheets", "slides", "hwp", "pdf", "markdown", "html", "mindmap", "diagram",
];

/// `RecoveryEditorKind`. Not checked by the type: the TypeScript checks it at
/// run time (`assertKind`) because a kind may come from outside.
pub type RecoveryEditorKind = String;

/// copies whose file was untouched this long are removed at launch (tunable)
pub const RECOVERY_COPY_MAX_AGE_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// `RecoveryCopyState`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryCopyState {
    /// written since the user last chose anything
    Undecided,
    /// the user closed the launch list without choosing
    Later,
    /// "restore" was chosen in the launch list, the tab is not loaded yet
    RestoreChosen,
}

impl RecoveryCopyState {
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryCopyState::Undecided => "undecided",
            RecoveryCopyState::Later => "later",
            RecoveryCopyState::RestoreChosen => "restoreChosen",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "undecided" => Some(RecoveryCopyState::Undecided),
            "later" => Some(RecoveryCopyState::Later),
            "restoreChosen" => Some(RecoveryCopyState::RestoreChosen),
            _ => None,
        }
    }
}

/// a document with a file on disk
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryFileIdentity {
    pub kind: RecoveryEditorKind,
    pub file_path: String,
}

/// a document that was never saved; `ext` is the format its copy is written in
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryUntitledIdentity {
    pub kind: RecoveryEditorKind,
    pub untitled_id: String,
    /// lower-case, with the dot: '.docx', '.hwpx', '.md'
    pub ext: String,
}

/// `RecoveryIdentity`
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryIdentity {
    File(RecoveryFileIdentity),
    Untitled(RecoveryUntitledIdentity),
}

impl RecoveryIdentity {
    pub fn kind(&self) -> &str {
        match self {
            RecoveryIdentity::File(f) => &f.kind,
            RecoveryIdentity::Untitled(u) => &u.kind,
        }
    }
}

/// `RecoveryDeleteReason`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDeleteReason {
    Saved,
    DontSave,
    Discarded,
    ClosedClean,
}

impl RecoveryDeleteReason {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "saved" => Some(RecoveryDeleteReason::Saved),
            "dontSave" => Some(RecoveryDeleteReason::DontSave),
            "discarded" => Some(RecoveryDeleteReason::Discarded),
            "closedClean" => Some(RecoveryDeleteReason::ClosedClean),
            _ => None,
        }
    }
}

/// `RecoveryLaunchDecision`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryLaunchDecision {
    Restore,
    Discard,
    Later,
}

impl RecoveryLaunchDecision {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "restore" => Some(RecoveryLaunchDecision::Restore),
            "discard" => Some(RecoveryLaunchDecision::Discard),
            "later" => Some(RecoveryLaunchDecision::Later),
            _ => None,
        }
    }
}

/// `RecoveryCopyInfo`
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryCopyInfo {
    pub identity: RecoveryIdentity,
    pub kind: RecoveryEditorKind,
    /// `None` for an untitled document
    pub file_path: Option<String>,
    pub untitled_id: Option<String>,
    /// tab title at the last write; the file name for a legacy file copy
    pub title: String,
    /// last write in ms since the epoch (the copy's mtime for a legacy copy)
    pub written_at: f64,
    /// the run that wrote the copy; '' for a legacy copy
    pub run_id: String,
    pub state: RecoveryCopyState,
    /// true when the copy has no metadata (left by an earlier version)
    pub legacy: bool,
    pub copy_path: String,
    pub copy_mtime_ms: f64,
    /// the file on disk was modified after the copy was written
    pub file_changed_after_copy: bool,
    /// a path is known but no file is there
    pub original_missing: bool,
    /// an untitled copy's original file
    pub original_path: Option<String>,
    /// an untitled copy's ODS formula loss
    pub formulas_dropped: Option<f64>,
}

/// `RecoveryOpenPlan`
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryOpenPlan {
    None,
    Restore(RecoveryCopyInfo),
    Ask(RecoveryCopyInfo),
}

/// `RecoveryWriteTicket`: a snapshot of a document's delete epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryWriteTicket {
    pub key: String,
    pub epoch: u64,
}

/// `RecoveryWriteResult`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryWriteResult {
    Written,
    Superseded,
}

/// `deleteCopy`'s answer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDeleteResult {
    Deleted,
    Refused,
}

pub struct RecoveryDeletePlan {
    copy: String,
    meta: String,
}

impl RecoveryDeletePlan {
    pub fn remove_authorized(
        &self,
        admit: &mut super::atomic_write::WriteAdmission<'_>,
    ) -> Result<(), NodeError> {
        for path in [&self.copy, &self.meta] {
            let mut result = None;
            admit(&mut || {
                result = Some(remove_recovery_file(path));
            })?;
            result.ok_or_else(|| plain("recovery deletion was not admitted"))??;
        }
        Ok(())
    }

    pub fn remove_owned(
        self,
        admit: &mut dyn FnMut(Box<dyn FnOnce() + Send>) -> Result<(), NodeError>,
    ) -> Result<(), NodeError> {
        for path in [self.copy, self.meta] {
            let result = std::sync::Arc::new(std::sync::Mutex::new(None));
            let captured = result.clone();
            admit(Box::new(move || {
                let outcome = remove_recovery_file(&path);
                if let Ok(mut slot) = captured.lock() {
                    *slot = Some(outcome);
                }
            }))?;
            let outcome = result
                .lock()
                .map_err(|_| plain("recovery deletion result is unavailable"))?
                .take()
                .ok_or_else(|| plain("recovery deletion was not admitted"))?;
            outcome?;
        }
        Ok(())
    }
}

pub struct RecoveryWritePlan {
    copy: String,
    metadata: JsonValue,
    ticket: RecoveryWriteTicket,
}

type RecoveryCopyWriter<'a> = dyn FnMut(&str, &[u8]) -> Result<(), NodeError> + 'a;

/// Immutable payload prepared after serialization and before entering the
/// identity FIFO. It is not a publication or a recovery-generation witness.
pub struct OwnedPreparedRecoveryWrite {
    plan: RecoveryWritePlan,
    bytes: Vec<u8>,
    metadata_bytes: Vec<u8>,
    queue: std::sync::Arc<pair_queue::RecoveryPairQueue>,
    pair_ticket: pair_queue::RecoveryPairTicket,
}

impl OwnedPreparedRecoveryWrite {
    #[allow(clippy::type_complexity)]
    pub fn new(
        plan: RecoveryWritePlan,
        bytes: Vec<u8>,
        pair_ticket: pair_queue::RecoveryPairTicket,
    ) -> Result<
        Self,
        Box<(
            NodeError,
            RecoveryWritePlan,
            Vec<u8>,
            pair_queue::RecoveryPairTicket,
        )>,
    > {
        let queue = match pair_queue::for_copy_path(&plan.copy) {
            Ok(queue) => queue,
            Err(error) => return Err(Box::new((error, plan, bytes, pair_ticket))),
        };
        if !pair_ticket.belongs_to(&queue) {
            return Err(Box::new((
                plain("recovery write ticket belongs to another identity"),
                plan,
                bytes,
                pair_ticket,
            )));
        }
        let metadata_bytes = jscompat::json_stringify_pretty(&plan.metadata, 2).into_bytes();
        Ok(Self {
            plan,
            bytes,
            metadata_bytes,
            queue,
            pair_ticket,
        })
    }

    pub fn copy_path(&self) -> &str {
        &self.plan.copy
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn metadata_bytes(&self) -> &[u8] {
        &self.metadata_bytes
    }
    pub fn original_ticket(&self) -> &RecoveryWriteTicket {
        &self.plan.ticket
    }
    pub fn pair_ticket(&self) -> &pair_queue::RecoveryPairTicket {
        &self.pair_ticket
    }
    pub fn queue(&self) -> &std::sync::Arc<pair_queue::RecoveryPairQueue> {
        &self.queue
    }

    /// Submit the immutable payload without losing it or its callback if the
    /// queue rejects the request. The callback runs on the queue's worker and
    /// must retain the turn through its final core continuation.
    pub fn submit<F>(self, job: F) -> Result<(), Box<(NodeError, Self, F)>>
    where
        F: FnOnce(Self, pair_queue::RecoveryPairTurn) + Send + 'static,
    {
        let queue = self.queue.clone();
        queue.submit_owned(self, job).map_err(Box::new)
    }

    pub fn intent(
        &self,
        generation: String,
        previous: Option<&RecoveryPinnedSnapshot>,
    ) -> Result<pair_journal::PairIntent, NodeError> {
        if previous.is_some_and(|snapshot| queue_key(&snapshot.path) != queue_key(&self.plan.copy))
        {
            return Err(plain(
                "the previous recovery snapshot belongs to another identity",
            ));
        }
        let previous = previous
            .map(pair_journal::PairImage::from_snapshot)
            .transpose()?;
        Ok(pair_journal::PairIntent {
            generation,
            previous,
            next: pair_journal::PairImage::from_bytes(
                &self.bytes,
                Some(self.metadata_bytes.clone()),
            ),
        })
    }
}

impl RecoveryWritePlan {
    pub fn ticket(&self) -> &RecoveryWriteTicket {
        &self.ticket
    }

    pub fn write_authorized(
        &self,
        bytes: &[u8],
        admit: &mut super::atomic_write::WriteAdmission<'_>,
    ) -> Result<(), NodeError> {
        let directory = dirname(&self.copy);
        let mut created = None;
        admit(&mut || {
            created = Some(fs::create_dir_all(&directory));
        })?;
        created
            .ok_or_else(|| plain("recovery directory creation was not admitted"))?
            .map_err(|error| node_error(&error, "mkdir", &directory))?;
        let admission = RefCell::new(admit);
        self.write_via_owned(
            bytes,
            &mut |path, metadata| {
                super::atomic_write::write_json_atomic_authorized(
                    path,
                    metadata,
                    &mut **admission.borrow_mut(),
                )
            },
            &mut |path, bytes| {
                super::atomic_write::atomic_write_file_authorized(
                    path,
                    bytes,
                    &mut **admission.borrow_mut(),
                )
            },
        )
    }

    pub fn write_via(
        &self,
        bytes: &[u8],
        write_metadata: &mut dyn FnMut(&str, &JsonValue) -> Result<(), NodeError>,
        write_copy: &mut RecoveryCopyWriter<'_>,
    ) -> Result<(), NodeError> {
        let directory = dirname(&self.copy);
        fs::create_dir_all(&directory).map_err(|error| node_error(&error, "mkdir", &directory))?;
        self.write_via_owned(bytes, write_metadata, write_copy)
    }

    pub fn write_via_owned(
        &self,
        bytes: &[u8],
        write_metadata: &mut dyn FnMut(&str, &JsonValue) -> Result<(), NodeError>,
        write_copy: &mut RecoveryCopyWriter<'_>,
    ) -> Result<(), NodeError> {
        write_metadata(&RecoveryStore::meta_path_of(&self.copy), &self.metadata)?;
        write_copy(&self.copy, bytes)
    }
}

/// Identity of the recovery copy object observed when a plan was made.
/// A replacement publishes a new file object, so the inode (or the creation
/// time off Unix) differs even when bytes and timestamps are identical.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct EntryStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}

impl EntryStamp {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                len: meta.len(),
                modified: meta.modified().ok(),
                device: meta.dev(),
                inode: meta.ino(),
                changed: (meta.ctime(), meta.ctime_nsec()),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                len: meta.len(),
                modified: meta.modified().ok(),
                created: meta.created().ok(),
            }
        }
    }

    fn at_path(path: &str) -> Option<Self> {
        fs::metadata(path).ok().map(|meta| Self::of(&meta))
    }
}

/// The plan rewrites only the metadata entry, so it pins only the copy object:
/// our own metadata republication (for example `mark_loaded`) of the same copy
/// must not invalidate it, while a replacement copy must.
pub struct RecoveryMetadataPlan {
    path: String,
    metadata: JsonValue,
    copy_path: String,
    expected_copy: Option<EntryStamp>,
}

impl RecoveryMetadataPlan {
    /// False once the copy was replaced or changed after the plan was made.
    fn still_current(&self) -> bool {
        EntryStamp::at_path(&self.copy_path) == self.expected_copy
    }

    /// `Ok(false)`: the plan was stale and nothing was written.
    pub fn write_via(
        &self,
        write: &mut dyn FnMut(&str, &JsonValue) -> Result<bool, NodeError>,
    ) -> Result<bool, NodeError> {
        if !self.still_current() {
            return Ok(false);
        }
        write(&self.path, &self.metadata)
    }

    /// `Ok(false)`: the plan was stale or the write failed; nothing was admitted.
    pub fn write_authorized(
        &self,
        admit: &mut super::atomic_write::WriteAdmission<'_>,
    ) -> Result<bool, NodeError> {
        if !self.still_current() {
            return Ok(false);
        }
        let mut rejected = false;
        let result = super::atomic_write::write_json_atomic_authorized(
            &self.path,
            &self.metadata,
            &mut |effect| {
                admit(effect).inspect_err(|_| {
                    rejected = true;
                })
            },
        );
        if rejected {
            result.map(|()| true)
        } else {
            Ok(result.is_ok())
        }
    }
}

/// `RecoveryWriteOptions`
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecoveryWriteOptions {
    pub title: Option<String>,
    pub ticket: Option<RecoveryWriteTicket>,
    /// an untitled copy's original file; ignored for a file copy
    pub original_path: Option<String>,
    /// an untitled copy's ODS formula loss; ignored for a file copy
    pub formulas_dropped: Option<f64>,
}

/// `RecoveryCopyFilter`
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecoveryCopyFilter {
    pub states: Option<Vec<RecoveryCopyState>>,
    pub kinds: Option<Vec<RecoveryEditorKind>>,
    /// true: untitled copies only; false: file copies only
    pub untitled: Option<bool>,
}

/// `adoptUntitledCopy`'s options
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryAdoptOptions {
    pub title: String,
    pub ext: String,
    pub state: Option<RecoveryCopyState>,
    pub original_path: Option<String>,
}

pub struct RecoveryAdoptPlan {
    source: String,
    target: String,
    identity: RecoveryUntitledIdentity,
    metadata: JsonValue,
}

#[derive(Clone, Copy)]
enum RecoveryAdoptAction<'a> {
    MakeDirectory(&'a str),
    WriteMetadata(&'a str, &'a JsonValue),
    Rename(&'a str, &'a str),
    Remove(&'a str),
}

fn create_adoption_directory(
    path: &str,
    admit: &mut dyn FnMut(Box<dyn FnOnce() -> bool + Send>) -> Result<bool, NodeError>,
) -> Result<bool, NodeError> {
    let mut missing = Vec::new();
    for directory in std::path::Path::new(path).ancestors() {
        if directory.as_os_str().is_empty() || directory.is_dir() {
            break;
        }
        if directory.exists() {
            return Ok(false);
        }
        missing.push(directory.to_path_buf());
    }
    for directory in missing.into_iter().rev() {
        let effect_path = directory.clone();
        if !admit(Box::new(move || fs::create_dir(effect_path).is_ok()))? && !directory.is_dir() {
            return Ok(false);
        }
    }
    Ok(true)
}

impl RecoveryAdoptPlan {
    pub fn prepare(
        user_data_dir: &str,
        kind: &str,
        source_path: &str,
        options: &RecoveryAdoptOptions,
        uuid: &dyn Fn() -> String,
        now: &dyn Fn() -> f64,
    ) -> Result<Option<Self>, NodeError> {
        assert_kind(kind)?;
        let folder = resolve(&[&join(&[user_data_dir, &recovery_folder_name(kind)])]);
        let source = resolve(&[source_path]);
        let inside = relative(&folder, &source);
        let separator = std::path::MAIN_SEPARATOR;
        let up = inside == ".."
            || inside.starts_with(&format!("..{separator}"))
            || (separator == '\\' && inside.starts_with("../"));
        if inside.is_empty()
            || up
            || is_absolute(&inside)
            || !std::path::Path::new(&source).exists()
        {
            return Ok(None);
        }
        let identity = RecoveryUntitledIdentity {
            kind: kind.to_owned(),
            untitled_id: new_untitled_recovery_id(uuid),
            ext: options.ext.clone(),
        };
        let as_identity = RecoveryIdentity::Untitled(identity.clone());
        let target = recovery_copy_path(user_data_dir, &as_identity)?;
        let metadata = RecoveryCopyMeta {
            kind: kind.to_owned(),
            file_path: None,
            untitled_id: Some(identity.untitled_id.clone()),
            title: options.title.clone(),
            written_at: stat_mtime(&source).unwrap_or_else(now),
            run_id: String::new(),
            state: options.state.unwrap_or(RecoveryCopyState::Later),
            original_path: original_path_field(&as_identity, options.original_path.as_deref()),
            formulas_dropped: None,
        }
        .to_json();
        Ok(Some(Self {
            source,
            target,
            identity,
            metadata,
        }))
    }

    fn publish_with(
        self,
        run: &mut dyn FnMut(RecoveryAdoptAction<'_>) -> Result<bool, NodeError>,
    ) -> Result<Option<RecoveryUntitledIdentity>, NodeError> {
        let dir = dirname(&self.target);
        let meta_path = RecoveryStore::meta_path_of(&self.target);
        if !run(RecoveryAdoptAction::MakeDirectory(&dir))?
            || !run(RecoveryAdoptAction::WriteMetadata(
                &meta_path,
                &self.metadata,
            ))?
            || !run(RecoveryAdoptAction::Rename(&self.source, &self.target))?
        {
            run(RecoveryAdoptAction::Remove(&meta_path))?;
            return Ok(None);
        }
        Ok(Some(self.identity))
    }

    pub fn publish_authorized(
        self,
        admit: &mut super::atomic_write::WriteAdmission<'_>,
    ) -> Result<Option<RecoveryUntitledIdentity>, NodeError> {
        self.publish_with(&mut |action| match action {
            RecoveryAdoptAction::WriteMetadata(path, metadata) => {
                let mut rejected = false;
                let result = super::atomic_write::write_json_atomic_authorized(
                    path,
                    metadata,
                    &mut |effect| {
                        admit(effect).inspect_err(|_| {
                            rejected = true;
                        })
                    },
                );
                if rejected {
                    return match result {
                        Err(error) => Err(error),
                        Ok(()) => Err(plain("recovery adoption authority was revoked")),
                    };
                }
                Ok(result.is_ok())
            }
            RecoveryAdoptAction::MakeDirectory(path) => {
                create_adoption_directory(path, &mut |effect| {
                    let mut effect = Some(effect);
                    let mut result = None;
                    let mut repeated = false;
                    admit(&mut || {
                        if let Some(effect) = effect.take() {
                            result = Some(effect());
                        } else {
                            repeated = true;
                        }
                    })?;
                    if repeated {
                        return Err(plain(
                            "recovery adoption effect was admitted more than once",
                        ));
                    }
                    result.ok_or_else(|| plain("recovery adoption effect was not admitted"))
                })
            }
            other => {
                let mut result = None;
                admit(&mut || {
                    result = Some(match other {
                        RecoveryAdoptAction::MakeDirectory(_) => unreachable!(),
                        RecoveryAdoptAction::Rename(source, target) => {
                            fs::rename(source, target).is_ok()
                        }
                        RecoveryAdoptAction::Remove(path) => {
                            remove_quietly(path);
                            true
                        }
                        RecoveryAdoptAction::WriteMetadata(_, _) => unreachable!(),
                    });
                })?;
                result.ok_or_else(|| plain("recovery adoption effect was not admitted"))
            }
        })
    }

    pub fn publish_via_owned(
        self,
        admit: &mut dyn FnMut(Box<dyn FnOnce() -> bool + Send>) -> Result<bool, NodeError>,
        write_metadata: &mut dyn FnMut(&str, &JsonValue) -> Result<bool, NodeError>,
    ) -> Result<Option<RecoveryUntitledIdentity>, NodeError> {
        self.publish_with(&mut |action| match action {
            RecoveryAdoptAction::WriteMetadata(path, metadata) => write_metadata(path, metadata),
            RecoveryAdoptAction::MakeDirectory(path) => create_adoption_directory(path, admit),
            RecoveryAdoptAction::Rename(source, target) => {
                let source = source.to_owned();
                let target = target.to_owned();
                admit(Box::new(move || fs::rename(source, target).is_ok()))
            }
            RecoveryAdoptAction::Remove(path) => {
                let path = path.to_owned();
                admit(Box::new(move || {
                    remove_quietly(&path);
                    true
                }))
            }
        })
    }
}

// ---- process-wide state ----

/// `SharedRecoveryState`: the shutdown latch, delete epochs, the run id and
/// the "Don't Save" block. (The write queues of the TypeScript are not
/// needed: see the module comment.)
pub struct SharedRecoveryState {
    shutting_down: Cell<bool>,
    epochs: RefCell<HashMap<String, u64>>,
    run_id: String,
    /// documents (by write queue key) discarded with "Don't Save", see `delete_copy`
    discarded: RefCell<HashSet<String>>,
}

impl SharedRecoveryState {
    /// a state with the given run id (`randomBytes(8).toString('hex')` in the TypeScript)
    pub fn new(run_id: impl Into<String>) -> Self {
        SharedRecoveryState {
            shutting_down: Cell::new(false),
            epochs: RefCell::new(HashMap::new()),
            run_id: run_id.into(),
            discarded: RefCell::new(HashSet::new()),
        }
    }

    /// a state with a random run id: 16 hex digits
    pub fn with_random_run_id() -> Self {
        let hex: String = random_uuid()
            .chars()
            .filter(|c| *c != '-')
            .take(16)
            .collect();
        Self::new(hex)
    }
}

/// this process's run id, recorded in every copy it writes
pub fn recovery_run_id(shared: &SharedRecoveryState) -> &str {
    &shared.run_id
}

/// The app is ending: from now on the store refuses deletions except a
/// successful save and an explicit "Don't Save".
pub fn mark_app_shutting_down(shared: &SharedRecoveryState) {
    shared.shutting_down.set(true);
}

/// the quit was vetoed: deletions work normally again
pub fn reset_app_shutting_down(shared: &SharedRecoveryState) {
    shared.shutting_down.set(false);
}

pub fn is_app_shutting_down(shared: &SharedRecoveryState) -> bool {
    shared.shutting_down.get()
}

/// A close was cancelled: every document discarded with "Don't Save" while it
/// was being answered is still open with its edits, so its copies resume.
pub fn resume_discarded_copies(shared: &SharedRecoveryState) {
    shared.discarded.borrow_mut().clear();
}

// ---- addressing ----

const META_SUFFIX: &str = ".meta.json";
const UNTITLED_DIR: &str = "untitled";

fn ext_matches(text: &str) -> bool {
    js_regex!(r"^\.[a-z0-9]{1,10}$").test(text)
}

fn untitled_id_matches(text: &str) -> bool {
    js_regex!(r"^[A-Za-z0-9_-]{1,64}$").test(text)
}

fn file_copy_matches(name: &str) -> bool {
    js_regex!(r"^[0-9a-f]{16}\.[a-z0-9]{1,10}$").test(name)
}

fn untitled_copy_matches(name: &str) -> bool {
    js_regex!(r"^[A-Za-z0-9_-]{1,64}\.[a-z0-9]{1,10}$").test(name)
}

fn temp_matches(name: &str) -> bool {
    js_regex!(r"^.+\.[0-9a-f]{12}\.tmp$").test(name)
}

fn kind_default_ext(kind: &str) -> &'static str {
    match kind {
        "docs" => ".docx",
        "sheets" => ".xlsx",
        "slides" => ".pptx",
        "hwp" => ".hwp",
        "pdf" => ".pdf",
        "markdown" => ".md",
        "html" => ".html",
        "mindmap" => ".xmind",
        _ => ".drawio",
    }
}

pub fn recovery_folder_name(kind: &str) -> String {
    format!("{kind}-autosave")
}

/// a fresh id for an untitled document's copy; `random_uuid` is `crypto.randomUUID`
pub fn new_untitled_recovery_id(random_uuid: &dyn Fn() -> String) -> String {
    random_uuid()
}

pub fn is_untitled_identity(identity: &RecoveryIdentity) -> bool {
    matches!(identity, RecoveryIdentity::Untitled(_))
}

/// whether a kind is one of the editors that keep recovery copies
pub fn is_recovery_editor_kind(value: &str) -> bool {
    RECOVERY_EDITOR_KINDS.contains(&value)
}

fn path_hash(file_path: &str) -> String {
    sha1_hex(file_path.as_bytes())[..16].to_owned()
}

/// the copy's extension for a file document
fn file_copy_ext(kind: &str, file_path: &str) -> String {
    match kind {
        "docs" => {
            if js_regex!(r"\.odt$", "i").test(file_path) {
                ".odt".to_owned()
            } else {
                ".docx".to_owned()
            }
        }
        "sheets" => ".xlsx".to_owned(),
        "slides" => ".pptx".to_owned(),
        _ => {
            let ext = extname(file_path).to_lowercase();
            if ext_matches(&ext) {
                ext
            } else {
                kind_default_ext(kind).to_owned()
            }
        }
    }
}

fn plain(message: impl Into<String>) -> NodeError {
    NodeError {
        code: None,
        message: message.into(),
    }
}

fn assert_kind(kind: &str) -> Result<(), NodeError> {
    if !is_recovery_editor_kind(kind) {
        return Err(plain(format!("unknown recovery editor kind: {kind}")));
    }
    Ok(())
}

fn assert_untitled(untitled_id: &str, ext: &str) -> Result<(), NodeError> {
    if !untitled_id_matches(untitled_id) {
        return Err(plain("invalid untitled recovery id"));
    }
    if !ext_matches(ext) {
        return Err(plain("invalid recovery copy extension"));
    }
    Ok(())
}

/// copy file name inside the kind folder (untitled ones under `untitled/`)
fn copy_relative_path(identity: &RecoveryIdentity) -> Result<String, NodeError> {
    assert_kind(identity.kind())?;
    match identity {
        RecoveryIdentity::Untitled(u) => {
            assert_untitled(&u.untitled_id, &u.ext)?;
            Ok(join(&[
                UNTITLED_DIR,
                &format!("{}{}", u.untitled_id, u.ext),
            ]))
        }
        RecoveryIdentity::File(f) => {
            if f.file_path.is_empty() {
                return Err(plain("recovery identity needs a file path"));
            }
            Ok(format!(
                "{}{}",
                path_hash(&f.file_path),
                file_copy_ext(&f.kind, &f.file_path)
            ))
        }
    }
}

pub fn recovery_copy_path(
    user_data_dir: &str,
    identity: &RecoveryIdentity,
) -> Result<String, NodeError> {
    Ok(join(&[
        user_data_dir,
        &recovery_folder_name(identity.kind()),
        &copy_relative_path(identity)?,
    ]))
}

pub struct RecoveryPinnedRead {
    identity: RecoveryIdentity,
    path: String,
    copy: fs::File,
    metadata: Option<fs::File>,
    metadata_error: Option<String>,
    parent: Option<fs::File>,
}

pub struct RecoveryPinnedSnapshot {
    pub bytes: Vec<u8>,
    pub original_path: Option<String>,
    pub formulas_dropped: Option<f64>,
    identity: RecoveryIdentity,
    path: String,
    meta: Option<RecoveryCopyMeta>,
    metadata_bytes: Option<Vec<u8>>,
    copy_mtime_ms: Option<f64>,
    copy_handle: fs::File,
    metadata_handle: Option<fs::File>,
    metadata_error: Option<String>,
    parent_handle: Option<fs::File>,
}

/// Saved-document recovery retains the exact copy read, not merely its path.
pub enum SavedRecoveryOpenResult {
    Original,
    Restore {
        snapshot: std::sync::Arc<RecoveryPinnedSnapshot>,
        copy: Box<RecoveryCopyInfo>,
    },
}

impl RecoveryPinnedSnapshot {
    pub fn copy_handle(&self) -> &fs::File {
        &self.copy_handle
    }
    pub fn metadata_handle(&self) -> Option<&fs::File> {
        self.metadata_handle.as_ref()
    }
    pub fn parent_handle(&self) -> Option<&fs::File> {
        self.parent_handle.as_ref()
    }
    pub fn exact_metadata(&self) -> Result<Option<&[u8]>, NodeError> {
        if let Some(error) = &self.metadata_error {
            return Err(plain(error.clone()));
        }
        match (&self.metadata_handle, &self.metadata_bytes) {
            (None, _) => Ok(None),
            (Some(_), Some(bytes)) => Ok(Some(bytes)),
            (Some(_), None) => Err(plain("the captured recovery metadata could not be read")),
        }
    }
    /// Describe the exact pinned read. `original_mtime_ms` comes from the
    /// retained committed object, never a later lookup of its selected path.
    pub fn copy_info(&self, original_mtime_ms: Option<f64>) -> Option<RecoveryCopyInfo> {
        let copy_mtime_ms = self.copy_mtime_ms?;
        let file_path = match &self.identity {
            RecoveryIdentity::File(file) => Some(file.file_path.clone()),
            RecoveryIdentity::Untitled(_) => None,
        };
        Some(RecoveryCopyInfo {
            identity: self.identity.clone(),
            kind: self.identity.kind().to_owned(),
            title: self
                .meta
                .as_ref()
                .map(|meta| meta.title.clone())
                .unwrap_or_else(|| {
                    file_path
                        .as_deref()
                        .map_or_else(String::new, |path| basename(path, None))
                }),
            file_path,
            untitled_id: match &self.identity {
                RecoveryIdentity::Untitled(value) => Some(value.untitled_id.clone()),
                _ => None,
            },
            written_at: self
                .meta
                .as_ref()
                .map_or(copy_mtime_ms, |meta| meta.written_at),
            run_id: self
                .meta
                .as_ref()
                .map_or_else(String::new, |meta| meta.run_id.clone()),
            state: self
                .meta
                .as_ref()
                .map_or(RecoveryCopyState::Later, |meta| meta.state),
            legacy: self.meta.is_none(),
            copy_path: self.path.clone(),
            copy_mtime_ms,
            file_changed_after_copy: original_mtime_ms.is_some_and(|mtime| mtime > copy_mtime_ms),
            original_missing: matches!(&self.identity, RecoveryIdentity::File(_))
                && original_mtime_ms.is_none(),
            original_path: self.original_path.clone(),
            formulas_dropped: self.formulas_dropped,
        })
    }

    pub fn prepare_copy_state(&self, state: RecoveryCopyState) -> Option<RecoveryMetadataPlan> {
        let copy_mtime_ms = self.copy_mtime_ms?;
        let file_path = match &self.identity {
            RecoveryIdentity::File(file) => Some(file.file_path.clone()),
            RecoveryIdentity::Untitled(_) => None,
        };
        let metadata = RecoveryCopyMeta {
            kind: self.identity.kind().to_owned(),
            file_path: file_path.clone(),
            untitled_id: match &self.identity {
                RecoveryIdentity::Untitled(untitled) => Some(untitled.untitled_id.clone()),
                RecoveryIdentity::File(_) => None,
            },
            title: self
                .meta
                .as_ref()
                .map(|meta| meta.title.clone())
                .unwrap_or_else(|| {
                    file_path
                        .as_deref()
                        .map_or_else(String::new, |path| basename(path, None))
                }),
            written_at: self
                .meta
                .as_ref()
                .map_or(copy_mtime_ms, |meta| meta.written_at),
            run_id: self
                .meta
                .as_ref()
                .map_or_else(String::new, |meta| meta.run_id.clone()),
            state,
            original_path: self.original_path.clone(),
            formulas_dropped: self.formulas_dropped,
        };
        Some(RecoveryMetadataPlan {
            path: RecoveryStore::meta_path_of(&self.path),
            metadata: metadata.to_json(),
            copy_path: self.path.clone(),
            expected_copy: self
                .copy_handle
                .metadata()
                .ok()
                .map(|meta| EntryStamp::of(&meta)),
        })
    }
}

impl RecoveryPinnedRead {
    /// Call only inside the target's short native seed admission.
    pub fn open(user_data_dir: &str, identity: &RecoveryIdentity) -> Result<Self, NodeError> {
        let path = recovery_copy_path(user_data_dir, identity)?;
        let copy = fs::File::open(&path).map_err(|error| node_error(&error, "open", &path))?;
        let metadata = fs::File::open(RecoveryStore::meta_path_of(&path)).ok();
        Ok(Self {
            identity: identity.clone(),
            path,
            copy,
            metadata,
            metadata_error: None,
            parent: None,
        })
    }

    /// Capture the app-private recovery directory and both entries relative
    /// to that same directory. This alone does not authorize mutation.
    #[cfg(any(unix, windows))]
    pub fn open_saved(user_data_dir: &str, identity: &RecoveryIdentity) -> Result<Self, NodeError> {
        use std::path::Path;

        let path = recovery_copy_path(user_data_dir, identity)?;
        let path_ref = Path::new(&path);
        let directory = path_ref
            .parent()
            .ok_or_else(|| plain("recovery copy has no parent directory"))?;
        let name = path_ref
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| plain("recovery copy has no Unicode entry name"))?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            };
            options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let parent = options
            .open(directory)
            .map_err(|error| node_error(&error, "open", &path))?;
        let parent_meta = parent
            .metadata()
            .map_err(|error| node_error(&error, "fstat", &path))?;
        if !parent_meta.is_dir() {
            return Err(plain("recovery parent is not a directory"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if parent_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(plain("recovery parent is a reparse point"));
            }
        }
        let copy = file_commit::open_pinned_file(&parent, name)
            .map_err(|error| node_error(&error, "open", &path))?;
        let meta_path = RecoveryStore::meta_path_of(&path);
        let meta_name = Path::new(&meta_path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| plain("recovery metadata has no Unicode entry name"))?;
        let (metadata, metadata_error) = match file_commit::open_pinned_file(&parent, meta_name) {
            Ok(file) => (Some(file), None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
            Err(error) => (None, Some(node_error(&error, "open", &meta_path).message)),
        };
        Ok(Self {
            identity: identity.clone(),
            path,
            copy,
            metadata,
            metadata_error,
            parent: Some(parent),
        })
    }

    /// Read pinned descriptors on a worker, without holding the seed gate.
    pub fn read(mut self) -> Result<RecoveryPinnedSnapshot, NodeError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        let copy_mtime_ms = self
            .copy
            .metadata()
            .ok()
            .map(|meta| crate::shell::recent_files::StatInfo::of(&meta).mtime_ms);
        self.copy
            .read_to_end(&mut bytes)
            .map_err(|error| node_error(&error, "read", &self.path))?;
        let metadata_bytes = self.metadata.as_mut().and_then(|file| {
            let mut data = Vec::new();
            file.read_to_end(&mut data).ok()?;
            Some(data)
        });
        let meta = metadata_bytes
            .as_ref()
            .and_then(|data| parse_meta(&String::from_utf8_lossy(data), &self.identity));
        Ok(RecoveryPinnedSnapshot {
            bytes,
            original_path: meta.as_ref().and_then(|meta| meta.original_path.clone()),
            formulas_dropped: meta.as_ref().and_then(|meta| meta.formulas_dropped),
            identity: self.identity,
            path: self.path,
            meta,
            metadata_bytes,
            copy_mtime_ms,
            copy_handle: self.copy,
            metadata_handle: self.metadata,
            metadata_error: self.metadata_error,
            parent_handle: self.parent,
        })
    }
}

fn queue_key(copy_path: &str) -> String {
    if cfg!(windows) {
        copy_path.to_lowercase()
    } else {
        copy_path.to_owned()
    }
}

// ---- metadata ----

/// `RecoveryCopyMeta`
struct RecoveryCopyMeta {
    kind: RecoveryEditorKind,
    file_path: Option<String>,
    untitled_id: Option<String>,
    title: String,
    written_at: f64,
    run_id: String,
    state: RecoveryCopyState,
    original_path: Option<String>,
    formulas_dropped: Option<f64>,
}

impl RecoveryCopyMeta {
    /// the object as `JSON.stringify` writes it: the keys in the order of the literal
    fn to_json(&self) -> JsonValue {
        let text = |s: &Option<String>| s.clone().map_or(JsonValue::Null, JsonValue::String);
        let mut meta = JsObject::new();
        meta.set("version", JsonValue::Number(1.0));
        meta.set("kind", JsonValue::String(self.kind.clone()));
        meta.set("filePath", text(&self.file_path));
        meta.set("untitledId", text(&self.untitled_id));
        meta.set("title", JsonValue::String(self.title.clone()));
        meta.set("writtenAt", JsonValue::Number(self.written_at));
        meta.set("runId", JsonValue::String(self.run_id.clone()));
        meta.set("state", JsonValue::String(self.state.as_str().to_owned()));
        if let Some(path) = &self.original_path {
            meta.set("originalPath", JsonValue::String(path.clone()));
        }
        if let Some(count) = self.formulas_dropped {
            meta.set("formulasDropped", JsonValue::Number(count));
        }
        JsonValue::Object(meta)
    }
}

/// `readFileSync(path, 'utf8')`, or `None` where it throws
fn read_text(path: &str) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// the metadata when it is readable and belongs to this identity, else None (legacy)
fn read_meta(meta_path: &str, identity: &RecoveryIdentity) -> Option<RecoveryCopyMeta> {
    parse_meta(&read_text(meta_path)?, identity)
}

fn parse_meta(text: &str, identity: &RecoveryIdentity) -> Option<RecoveryCopyMeta> {
    let raw = json_parse(text).ok()?;
    // an array has none of the keys below: every comparison fails
    let JsonValue::Object(meta) = raw else {
        return None;
    };
    let string = |key: &str| match meta.get(key) {
        Some(JsonValue::String(s)) => Some(s.clone()),
        _ => None,
    };
    if string("kind")? != identity.kind() {
        return None;
    }
    let state = RecoveryCopyState::parse(&string("state")?)?;
    let title = string("title")?;
    let run_id = string("runId")?;
    let written_at = match meta.get("writtenAt") {
        Some(JsonValue::Number(x)) if x.is_finite() => *x,
        _ => return None,
    };
    match identity {
        RecoveryIdentity::Untitled(u) => {
            if string("untitledId").as_deref() != Some(u.untitled_id.as_str())
                || meta.get("filePath") != Some(&JsonValue::Null)
            {
                return None;
            }
        }
        RecoveryIdentity::File(f) => {
            if string("filePath").as_deref() != Some(f.file_path.as_str())
                || meta.get("untitledId") != Some(&JsonValue::Null)
            {
                return None;
            }
        }
    }
    let original = string("originalPath");
    let dropped = match meta.get("formulasDropped") {
        Some(JsonValue::Number(x)) => Some(*x),
        _ => None,
    };
    Some(RecoveryCopyMeta {
        kind: identity.kind().to_owned(),
        file_path: string("filePath"),
        untitled_id: string("untitledId"),
        title,
        written_at,
        run_id,
        state,
        original_path: original_path_field(identity, original.as_deref()),
        formulas_dropped: formulas_dropped_field(identity, dropped),
    })
}

/// the metadata field for an untitled copy's original path (nothing otherwise)
fn original_path_field(identity: &RecoveryIdentity, original_path: Option<&str>) -> Option<String> {
    if !is_untitled_identity(identity) {
        return None;
    }
    original_path
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

/// the metadata field for an untitled copy's ODS formula loss (nothing otherwise)
fn formulas_dropped_field(
    identity: &RecoveryIdentity,
    formulas_dropped: Option<f64>,
) -> Option<f64> {
    if !is_untitled_identity(identity) {
        return None;
    }
    formulas_dropped.filter(|n| n.is_finite() && n.fract() == 0.0 && *n > 0.0)
}

/// the identity a metadata file next to `copy_name` describes, or None
fn identity_from_meta(
    kind: &str,
    meta_path: &str,
    copy_name: &str,
    untitled: bool,
) -> Result<Option<RecoveryIdentity>, NodeError> {
    let Some(text) = read_text(meta_path) else {
        return Ok(None);
    };
    let Ok(JsonValue::Object(meta)) = json_parse(&text) else {
        return Ok(None);
    };
    let ext = extname(copy_name);
    if untitled {
        let id = &copy_name[..copy_name.len() - ext.len()];
        return Ok(match meta.get("untitledId") {
            Some(JsonValue::String(s)) if s == id => {
                Some(RecoveryIdentity::Untitled(RecoveryUntitledIdentity {
                    kind: kind.to_owned(),
                    untitled_id: id.to_owned(),
                    ext,
                }))
            }
            _ => None,
        });
    }
    let file_path = match meta.get("filePath") {
        Some(JsonValue::String(s)) if !s.is_empty() => s.clone(),
        _ => return Ok(None),
    };
    let identity = RecoveryIdentity::File(RecoveryFileIdentity {
        kind: kind.to_owned(),
        file_path,
    });
    // the name must be the one this path hashes to, or the metadata is not this copy's
    Ok((copy_relative_path(&identity)? == copy_name).then_some(identity))
}

/// `statSync(path).mtimeMs`, or None where it throws
fn stat_mtime(path: &str) -> Option<f64> {
    let meta = fs::metadata(path).ok()?;
    Some(crate::shell::recent_files::StatInfo::of(&meta).mtime_ms)
}

fn remove_quietly(path: &str) -> bool {
    fs::remove_file(path).is_ok()
}

fn remove_recovery_file(path: &str) -> Result<(), NodeError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(node_error(&error, "unlink", path)),
    }
}

/// `readdirSync(dir)`: libuv sorts the names by bytes; None where it throws
fn read_dir_names(dir: &str) -> Option<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Some(names)
}

// ---- the store ----

/// `RecoveryStoreOptions`, with the clock and the random source the
/// TypeScript reads from `Date.now()` and `crypto.randomUUID()`.
pub struct RecoveryStoreOptions<'a> {
    /// the app data folder, read on every call
    pub user_data_dir: &'a dyn Fn() -> String,
    /// `Date.now()`
    pub now_ms: &'a dyn Fn() -> f64,
    /// `crypto.randomUUID()`
    pub random_uuid: &'a dyn Fn() -> String,
}

/// `createRecoveryStore(options)`
pub fn create_recovery_store<'a>(
    options: RecoveryStoreOptions<'a>,
    shared: &'a SharedRecoveryState,
) -> RecoveryStore<'a> {
    RecoveryStore { options, shared }
}

pub struct RecoveryStore<'a> {
    options: RecoveryStoreOptions<'a>,
    shared: &'a SharedRecoveryState,
}

type Land<'l> = &'l mut dyn FnMut(
    &str,
    &dyn Fn() -> Result<(), NodeError>,
    &dyn Fn() -> bool,
) -> Result<bool, NodeError>;

impl RecoveryStore<'_> {
    fn root_dir(&self) -> String {
        (self.options.user_data_dir)()
    }

    fn kind_dir(&self, kind: &str) -> String {
        join(&[&self.root_dir(), &recovery_folder_name(kind)])
    }

    fn meta_path_of(copy: &str) -> String {
        format!("{copy}{META_SUFFIX}")
    }

    fn epoch_of(&self, key: &str) -> u64 {
        self.shared.epochs.borrow().get(key).copied().unwrap_or(0)
    }

    fn bump_epoch(&self, key: &str) {
        let next = self.epoch_of(key) + 1;
        self.shared.epochs.borrow_mut().insert(key.to_owned(), next);
    }

    /// where this document's copy lives (whether or not it exists)
    pub fn copy_path(&self, identity: &RecoveryIdentity) -> Result<String, NodeError> {
        recovery_copy_path(&self.root_dir(), identity)
    }

    /// take a write ticket now
    pub fn begin_write(
        &self,
        identity: &RecoveryIdentity,
    ) -> Result<RecoveryWriteTicket, NodeError> {
        let key = queue_key(&self.copy_path(identity)?);
        let epoch = self.epoch_of(&key);
        Ok(RecoveryWriteTicket { key, epoch })
    }

    pub fn write_ticket_is_current(&self, ticket: &RecoveryWriteTicket) -> bool {
        self.epoch_of(&ticket.key) == ticket.epoch
            && !self.shared.discarded.borrow().contains(&ticket.key)
    }

    pub fn prepare_write(
        &self,
        identity: &RecoveryIdentity,
        options: &RecoveryWriteOptions,
    ) -> Result<Option<RecoveryWritePlan>, NodeError> {
        let copy = self.copy_path(identity)?;
        let ticket = match &options.ticket {
            Some(ticket) => ticket.clone(),
            None => self.begin_write(identity)?,
        };
        if ticket.key != queue_key(&copy) {
            return Err(plain("recovery write ticket belongs to another document"));
        }
        if !self.write_ticket_is_current(&ticket) {
            return Ok(None);
        }
        let metadata = RecoveryCopyMeta {
            kind: identity.kind().to_owned(),
            file_path: match identity {
                RecoveryIdentity::File(file) => Some(file.file_path.clone()),
                _ => None,
            },
            untitled_id: match identity {
                RecoveryIdentity::Untitled(file) => Some(file.untitled_id.clone()),
                _ => None,
            },
            title: options.title.clone().unwrap_or_else(|| match identity {
                RecoveryIdentity::File(file) => basename(&file.file_path, None),
                RecoveryIdentity::Untitled(_) => String::new(),
            }),
            written_at: (self.options.now_ms)(),
            run_id: recovery_run_id(self.shared).to_owned(),
            state: RecoveryCopyState::Undecided,
            original_path: original_path_field(identity, options.original_path.as_deref()),
            formulas_dropped: formulas_dropped_field(identity, options.formulas_dropped),
        }
        .to_json();
        Ok(Some(RecoveryWritePlan {
            copy,
            metadata,
            ticket,
        }))
    }

    pub fn prepare_delete(
        &self,
        identity: &RecoveryIdentity,
        reason: RecoveryDeleteReason,
    ) -> Result<Option<RecoveryDeletePlan>, NodeError> {
        if is_app_shutting_down(self.shared)
            && reason != RecoveryDeleteReason::Saved
            && reason != RecoveryDeleteReason::DontSave
        {
            return Ok(None);
        }
        let copy = self.copy_path(identity)?;
        let key = queue_key(&copy);
        self.bump_epoch(&key);
        if reason == RecoveryDeleteReason::DontSave {
            self.shared.discarded.borrow_mut().insert(key);
        }
        let meta = Self::meta_path_of(&copy);
        Ok(Some(RecoveryDeletePlan { copy, meta }))
    }

    fn remove_copy_files(copy: &str) {
        remove_quietly(copy);
        remove_quietly(&Self::meta_path_of(copy));
    }

    /// The common shape of writeCopy and writeCopyStreamed: check the ticket,
    /// run `land` inside the document's turn, and undo the result when a
    /// delete got in meanwhile. `land` gets the metadata writer.
    fn queue_copy_write(
        &self,
        identity: &RecoveryIdentity,
        write_options: &RecoveryWriteOptions,
        land: Land<'_>,
        admission: Option<&RefCell<&mut super::atomic_write::WriteAdmission<'_>>>,
    ) -> Result<RecoveryWriteResult, NodeError> {
        let copy = self.copy_path(identity)?;
        let key = queue_key(&copy);
        let ticket = write_options
            .ticket
            .clone()
            .unwrap_or_else(|| RecoveryWriteTicket {
                key: key.clone(),
                epoch: self.epoch_of(&key),
            });
        if ticket.key != key {
            return Err(plain("recovery write ticket belongs to another document"));
        }
        let title = match (&write_options.title, identity) {
            (Some(title), _) => title.clone(),
            (None, RecoveryIdentity::Untitled(_)) => String::new(),
            (None, RecoveryIdentity::File(f)) => basename(&f.file_path, None),
        };
        let superseded = || self.epoch_of(&key) != ticket.epoch;
        let write_meta = || -> Result<(), NodeError> {
            let meta = RecoveryCopyMeta {
                kind: identity.kind().to_owned(),
                file_path: match identity {
                    RecoveryIdentity::File(f) => Some(f.file_path.clone()),
                    RecoveryIdentity::Untitled(_) => None,
                },
                untitled_id: match identity {
                    RecoveryIdentity::Untitled(u) => Some(u.untitled_id.clone()),
                    RecoveryIdentity::File(_) => None,
                },
                title: title.clone(),
                written_at: (self.options.now_ms)(),
                run_id: recovery_run_id(self.shared).to_owned(),
                state: RecoveryCopyState::Undecided,
                original_path: original_path_field(
                    identity,
                    write_options.original_path.as_deref(),
                ),
                formulas_dropped: formulas_dropped_field(identity, write_options.formulas_dropped),
            };
            match admission {
                Some(admission) => super::atomic_write::write_json_atomic_authorized(
                    &Self::meta_path_of(&copy),
                    &meta.to_json(),
                    &mut **admission.borrow_mut(),
                ),
                None => write_json_atomic(&Self::meta_path_of(&copy), &meta.to_json()),
            }
        };
        // "Don't Save" holds until the close is cancelled or the document reopens
        if superseded() || self.shared.discarded.borrow().contains(&key) {
            return Ok(RecoveryWriteResult::Superseded);
        }
        let dir = dirname(&copy);
        fs::create_dir_all(&dir).map_err(|e| node_error(&e, "mkdir", &dir))?;
        if !land(&copy, &write_meta, &superseded)? {
            return Ok(RecoveryWriteResult::Superseded);
        }
        // a delete that ran while the bytes were landing must still win: undo this write
        if superseded() {
            if let Some(admission) = admission {
                let meta = Self::meta_path_of(&copy);
                let mut admit = admission.borrow_mut();
                admit(&mut || {
                    remove_quietly(&copy);
                })?;
                admit(&mut || {
                    remove_quietly(&meta);
                })?;
            } else {
                Self::remove_copy_files(&copy);
            }
            return Ok(RecoveryWriteResult::Superseded);
        }
        Ok(RecoveryWriteResult::Written)
    }

    /// Write (or replace) the copy atomically and mark it undecided.
    pub fn write_copy(
        &self,
        identity: &RecoveryIdentity,
        bytes: &[u8],
        write_options: &RecoveryWriteOptions,
    ) -> Result<RecoveryWriteResult, NodeError> {
        self.queue_copy_write(
            identity,
            write_options,
            &mut |copy, write_meta, _superseded| {
                // metadata first: a crash between the two leaves an orphan metadata file
                // (ignored), never an untitled copy without its title
                write_meta()?;
                atomic_write_file(copy, bytes)?;
                Ok(true)
            },
            None,
        )
    }

    pub fn write_copy_authorized(
        &self,
        identity: &RecoveryIdentity,
        bytes: &[u8],
        write_options: &RecoveryWriteOptions,
        admit: &mut super::atomic_write::WriteAdmission<'_>,
    ) -> Result<RecoveryWriteResult, NodeError> {
        let admission = RefCell::new(admit);
        self.queue_copy_write(
            identity,
            write_options,
            &mut |copy, write_meta, _| {
                write_meta()?;
                super::atomic_write::atomic_write_file_authorized(
                    copy,
                    bytes,
                    &mut **admission.borrow_mut(),
                )?;
                Ok(true)
            },
            Some(&admission),
        )
    }

    /// writeCopy for content too large to hold in memory: `write` streams the
    /// copy to the temp path it is given, and the store moves it into place.
    pub fn write_copy_streamed(
        &self,
        identity: &RecoveryIdentity,
        write: &mut dyn FnMut(&str) -> Result<(), NodeError>,
        write_options: &RecoveryWriteOptions,
    ) -> Result<RecoveryWriteResult, NodeError> {
        self.queue_copy_write(
            identity,
            write_options,
            &mut |copy, write_meta, superseded| {
                let temp = temp_path_beside(copy);
                let landed = (|| -> Result<bool, NodeError> {
                    write(&temp)?;
                    // a delete during the (long) serialization: the temp never becomes the copy
                    if superseded() {
                        remove_quietly(&temp);
                        return Ok(false);
                    }
                    // metadata first, as in writeCopy
                    write_meta()?;
                    atomic_move_file(&temp, copy)?;
                    Ok(true)
                })();
                if landed.is_err() {
                    remove_quietly(&temp);
                }
                landed
            },
            None,
        )
    }

    pub fn write_copy_streamed_authorized(
        &self,
        identity: &RecoveryIdentity,
        write: &mut dyn FnMut(&str) -> Result<(), NodeError>,
        write_options: &RecoveryWriteOptions,
        admit: &mut super::atomic_write::WriteAdmission<'_>,
    ) -> Result<RecoveryWriteResult, NodeError> {
        let admission = RefCell::new(admit);
        self.queue_copy_write(
            identity,
            write_options,
            &mut |copy, write_meta, superseded| {
                let temp = temp_path_beside(copy);
                let landed = (|| {
                    write(&temp)?;
                    if superseded() {
                        remove_quietly(&temp);
                        return Ok(false);
                    }
                    write_meta()?;
                    super::atomic_write::atomic_move_file_authorized(
                        &temp,
                        copy,
                        &mut **admission.borrow_mut(),
                    )?;
                    Ok(true)
                })();
                if landed.is_err() {
                    remove_quietly(&temp);
                }
                landed
            },
            Some(&admission),
        )
    }

    /// Delete the copy and its metadata now. While the app is ending, only
    /// `Saved` and `DontSave` are carried out.
    pub fn delete_copy(
        &self,
        identity: &RecoveryIdentity,
        reason: RecoveryDeleteReason,
    ) -> Result<RecoveryDeleteResult, NodeError> {
        let Some(plan) = self.prepare_delete(identity, reason)? else {
            return Ok(RecoveryDeleteResult::Refused);
        };
        Self::remove_copy_files(&plan.copy);
        Ok(RecoveryDeleteResult::Deleted)
    }

    /// Preserve delete ordering while admitting each namespace/state effect.
    pub fn delete_copy_authorized(
        &self,
        identity: &RecoveryIdentity,
        reason: RecoveryDeleteReason,
        admit: &mut super::atomic_write::WriteAdmission<'_>,
    ) -> Result<RecoveryDeleteResult, NodeError> {
        if is_app_shutting_down(self.shared)
            && reason != RecoveryDeleteReason::Saved
            && reason != RecoveryDeleteReason::DontSave
        {
            return Ok(RecoveryDeleteResult::Refused);
        }
        let mut prepared = Ok(None);
        admit(&mut || {
            prepared = self.prepare_delete(identity, reason);
        })?;
        let Some(plan) = prepared? else {
            return Ok(RecoveryDeleteResult::Refused);
        };
        plan.remove_authorized(admit)?;
        Ok(RecoveryDeleteResult::Deleted)
    }

    /// the document was discarded with "Don't Save" and takes no copy for now
    pub fn is_discarded(&self, identity: &RecoveryIdentity) -> Result<bool, NodeError> {
        let key = queue_key(&self.copy_path(identity)?);
        Ok(self.shared.discarded.borrow().contains(&key))
    }

    /// The document is open (again) with edits that are not the discarded ones.
    pub fn clear_discarded(&self, identity: &RecoveryIdentity) -> Result<(), NodeError> {
        let key = queue_key(&self.copy_path(identity)?);
        self.shared.discarded.borrow_mut().remove(&key);
        Ok(())
    }

    fn info_for(identity: &RecoveryIdentity, copy: &str) -> Option<RecoveryCopyInfo> {
        let copy_mtime_ms = stat_mtime(copy)?;
        let meta = read_meta(&Self::meta_path_of(copy), identity);
        let file_path = match identity {
            RecoveryIdentity::File(f) => Some(f.file_path.clone()),
            RecoveryIdentity::Untitled(_) => None,
        };
        let mut file_changed_after_copy = false;
        let mut original_missing = false;
        if let Some(path) = &file_path {
            match stat_mtime(path) {
                None => original_missing = true,
                Some(file_mtime) => file_changed_after_copy = file_mtime > copy_mtime_ms,
            }
        }
        Some(RecoveryCopyInfo {
            identity: identity.clone(),
            kind: identity.kind().to_owned(),
            title: match &meta {
                Some(m) => m.title.clone(),
                None => file_path
                    .as_deref()
                    .map_or_else(String::new, |p| basename(p, None)),
            },
            file_path,
            untitled_id: match identity {
                RecoveryIdentity::Untitled(u) => Some(u.untitled_id.clone()),
                RecoveryIdentity::File(_) => None,
            },
            written_at: meta.as_ref().map_or(copy_mtime_ms, |m| m.written_at),
            run_id: meta.as_ref().map_or_else(String::new, |m| m.run_id.clone()),
            state: meta.as_ref().map_or(RecoveryCopyState::Later, |m| m.state),
            legacy: meta.is_none(),
            copy_path: copy.to_owned(),
            copy_mtime_ms,
            file_changed_after_copy,
            original_missing,
            original_path: meta.as_ref().and_then(|m| m.original_path.clone()),
            formulas_dropped: meta.as_ref().and_then(|m| m.formulas_dropped),
        })
    }

    /// the copy's description, or None when there is none
    pub fn copy_info(
        &self,
        identity: &RecoveryIdentity,
    ) -> Result<Option<RecoveryCopyInfo>, NodeError> {
        Ok(Self::info_for(identity, &self.copy_path(identity)?))
    }

    /// the copy's bytes, or None when it cannot be read (`copyPath` throwing included)
    pub fn read_copy(&self, identity: &RecoveryIdentity) -> Option<Vec<u8>> {
        fs::read(self.copy_path(identity).ok()?).ok()
    }

    /// what opening the document does; the document is being opened, so a
    /// "Don't Save" block lifts
    pub fn open_plan(&self, identity: &RecoveryIdentity) -> Result<RecoveryOpenPlan, NodeError> {
        self.clear_discarded(identity)?;
        Ok(match self.copy_info(identity)? {
            None => RecoveryOpenPlan::None,
            Some(copy) if copy.state == RecoveryCopyState::RestoreChosen => {
                RecoveryOpenPlan::Restore(copy)
            }
            Some(copy) => RecoveryOpenPlan::Ask(copy),
        })
    }

    /// false when there is no copy
    pub fn set_copy_state(
        &self,
        identity: &RecoveryIdentity,
        state: RecoveryCopyState,
    ) -> Result<bool, NodeError> {
        let Some(plan) = self.prepare_copy_state_update(identity, state)? else {
            return Ok(false);
        };
        plan.write_via(&mut |path, metadata| Ok(write_json_atomic(path, metadata).is_ok()))
    }

    pub fn set_copy_state_authorized(
        &self,
        identity: &RecoveryIdentity,
        state: RecoveryCopyState,
        admit: &mut super::atomic_write::WriteAdmission<'_>,
    ) -> Result<bool, NodeError> {
        let Some(plan) = self.prepare_copy_state_update(identity, state)? else {
            return Ok(false);
        };
        plan.write_authorized(admit)
    }

    pub fn prepare_copy_state_update(
        &self,
        identity: &RecoveryIdentity,
        state: RecoveryCopyState,
    ) -> Result<Option<RecoveryMetadataPlan>, NodeError> {
        let copy = self.copy_path(identity)?;
        Ok(Self::prepare_copy_state_from_path(identity, &copy, state))
    }

    pub fn prepare_copy_state_from_path(
        identity: &RecoveryIdentity,
        copy: &str,
        state: RecoveryCopyState,
    ) -> Option<RecoveryMetadataPlan> {
        let info = Self::info_for(identity, copy)?;
        let meta = RecoveryCopyMeta {
            kind: identity.kind().to_owned(),
            file_path: info.file_path.clone(),
            untitled_id: match identity {
                RecoveryIdentity::Untitled(u) => Some(u.untitled_id.clone()),
                RecoveryIdentity::File(_) => None,
            },
            title: info.title.clone(),
            written_at: info.written_at,
            run_id: info.run_id.clone(),
            state,
            original_path: original_path_field(identity, info.original_path.as_deref()),
            formulas_dropped: formulas_dropped_field(identity, info.formulas_dropped),
        };
        Some(RecoveryMetadataPlan {
            path: Self::meta_path_of(copy),
            metadata: meta.to_json(),
            copy_path: copy.to_owned(),
            expected_copy: EntryStamp::at_path(copy),
        })
    }

    /// Record a launch-list choice: restore -> restoreChosen, later -> later,
    /// discard -> delete. False when nothing changed.
    pub fn apply_launch_decision(
        &self,
        identity: &RecoveryIdentity,
        decision: RecoveryLaunchDecision,
    ) -> Result<bool, NodeError> {
        if decision == RecoveryLaunchDecision::Discard {
            if self.copy_info(identity)?.is_none() {
                return Ok(false);
            }
            return Ok(self.delete_copy(identity, RecoveryDeleteReason::Discarded)?
                == RecoveryDeleteResult::Deleted);
        }
        self.set_copy_state(
            identity,
            if decision == RecoveryLaunchDecision::Restore {
                RecoveryCopyState::RestoreChosen
            } else {
                RecoveryCopyState::Later
            },
        )
    }

    /// The editor loaded the copy's content into a tab: the copy is
    /// undecided again (and a "Don't Save" block lifts).
    pub fn mark_copy_loaded(&self, identity: &RecoveryIdentity) -> Result<bool, NodeError> {
        self.clear_discarded(identity)?;
        self.set_copy_state(identity, RecoveryCopyState::Undecided)
    }

    /// Copies with metadata, oldest first.
    pub fn list_copies(
        &self,
        filter: &RecoveryCopyFilter,
    ) -> Result<Vec<RecoveryCopyInfo>, NodeError> {
        let default_kinds: Vec<String> = RECOVERY_EDITOR_KINDS
            .iter()
            .map(|k| (*k).to_owned())
            .collect();
        let kinds = filter.kinds.as_ref().unwrap_or(&default_kinds);
        let mut found: Vec<RecoveryCopyInfo> = Vec::new();
        for kind in kinds {
            let mut places: Vec<(String, bool)> = Vec::new();
            if filter.untitled != Some(true) {
                places.push((self.kind_dir(kind), false));
            }
            if filter.untitled != Some(false) {
                places.push((join(&[&self.kind_dir(kind), UNTITLED_DIR]), true));
            }
            for (dir, untitled) in places {
                let Some(names) = read_dir_names(&dir) else {
                    continue;
                };
                for name in names {
                    let is_copy = if untitled {
                        untitled_copy_matches(&name)
                    } else {
                        file_copy_matches(&name)
                    };
                    if !is_copy {
                        continue;
                    }
                    let copy = join(&[&dir, &name]);
                    let Some(identity) =
                        identity_from_meta(kind, &Self::meta_path_of(&copy), &name, untitled)?
                    else {
                        continue;
                    };
                    let Some(info) = Self::info_for(&identity, &copy) else {
                        continue;
                    };
                    if info.legacy {
                        continue;
                    }
                    if let Some(states) = &filter.states
                        && !states.contains(&info.state)
                    {
                        continue;
                    }
                    found.push(info);
                }
            }
        }
        // a stable sort, as Array.prototype.sort is
        found.sort_by(|a, b| {
            a.written_at
                .partial_cmp(&b.written_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(found)
    }

    /// any undecided or restoreChosen copy left (the normal-exit judgment)
    pub fn has_unresolved_copies(&self) -> Result<bool, NodeError> {
        let filter = RecoveryCopyFilter {
            states: Some(vec![
                RecoveryCopyState::Undecided,
                RecoveryCopyState::RestoreChosen,
            ]),
            ..RecoveryCopyFilter::default()
        };
        Ok(!self.list_copies(&filter)?.is_empty())
    }

    /// Remove copies (legacy ones and leftover temp files included) whose file
    /// was untouched for longer than `max_age_ms`. Answers how many were
    /// removed; does nothing while the app is ending.
    pub fn prune_stale_copies(&self, max_age_ms: Option<f64>) -> usize {
        if is_app_shutting_down(self.shared) {
            return 0;
        }
        let cutoff = (self.options.now_ms)() - max_age_ms.unwrap_or(RECOVERY_COPY_MAX_AGE_MS);
        let mut removed = 0;
        for kind in RECOVERY_EDITOR_KINDS {
            let kind_dir = self.kind_dir(kind);
            for (dir, untitled) in [
                (kind_dir.clone(), false),
                (join(&[&kind_dir, UNTITLED_DIR]), true),
            ] {
                let Some(names) = read_dir_names(&dir) else {
                    continue;
                };
                for name in names {
                    let path = join(&[&dir, &name]);
                    let is_copy = if untitled {
                        untitled_copy_matches(&name)
                    } else {
                        file_copy_matches(&name)
                    };
                    let is_temp = temp_matches(&name);
                    let is_meta = name.ends_with(META_SUFFIX);
                    if !is_copy && !is_temp && !is_meta {
                        continue;
                    }
                    let Some(mtime) = stat_mtime(&path) else {
                        continue;
                    };
                    if mtime >= cutoff {
                        continue;
                    }
                    if is_meta {
                        // an orphan (its copy already gone); a live copy's metadata goes with it
                        if !std::path::Path::new(&path[..path.len() - META_SUFFIX.len()]).exists() {
                            remove_quietly(&path);
                        }
                        continue;
                    }
                    if is_copy {
                        self.bump_epoch(&queue_key(&path));
                        remove_quietly(&Self::meta_path_of(&path));
                    }
                    if remove_quietly(&path) {
                        removed += 1;
                    }
                }
            }
        }
        removed
    }

    /// Move a copy that sits in the editor's own recovery folder into a new
    /// untitled slot. None, with the source left alone, when it is outside
    /// that folder or the move fails.
    pub fn adopt_untitled_copy(
        &self,
        kind: &str,
        source_path: &str,
        adopt_options: &RecoveryAdoptOptions,
    ) -> Result<Option<RecoveryUntitledIdentity>, NodeError> {
        let Some(plan) = RecoveryAdoptPlan::prepare(
            &self.root_dir(),
            kind,
            source_path,
            adopt_options,
            self.options.random_uuid,
            self.options.now_ms,
        )?
        else {
            return Ok(None);
        };
        plan.publish_authorized(&mut |effect| {
            effect();
            Ok(())
        })
    }
}

#[cfg(all(test, unix))]
mod pinned_snapshot_tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    #[test]
    fn snapshot_retains_original_copy_after_name_replacement() {
        let root =
            std::env::temp_dir().join(format!("recovery-pinned-{}", jscompat::random_uuid()));
        let identity = RecoveryIdentity::File(RecoveryFileIdentity {
            kind: "docs".into(),
            file_path: "/document.docx".into(),
        });
        let path = recovery_copy_path(root.to_str().unwrap(), &identity).unwrap();
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, b"original").unwrap();
        let pinned = RecoveryPinnedRead::open_saved(root.to_str().unwrap(), &identity).unwrap();
        let displaced = format!("{path}.old");
        std::fs::rename(&path, &displaced).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        let snapshot = pinned.read().unwrap();
        assert!(snapshot.parent_handle().is_some());
        assert_eq!(snapshot.bytes, b"original");
        let mut original = snapshot.copy_handle().try_clone().unwrap();
        original.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        original.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        std::fs::remove_dir_all(root).unwrap();
    }
}
