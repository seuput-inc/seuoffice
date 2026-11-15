//! The recovery-copy side of `slides-main.ts` that the channels touch
//! (what an open offers, what a save or a discarded close deletes) and the
//! recent files list. The 30 second copy writer (`runSlidesRecoveryPass`)
//! needs a timer and belongs to the shell.

use std::path::Path;

use jscompat::{JsonValue, json_parse};
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::recovery_prompt::{
    RecoveryOpenResult, ResolveRecoveryOnOpenOptions, resolve_recovery_on_open,
};
use main_core::packages::electron_utils::recovery_store::{
    RecoveryCopyState, RecoveryDeleteReason, RecoveryDeleteResult, RecoveryFileIdentity,
    RecoveryIdentity, RecoveryStore, RecoveryStoreOptions, RecoveryUntitledIdentity,
    RecoveryWriteOptions, RecoveryWriteResult, create_recovery_store, new_untitled_recovery_id,
};
use shell_contract::ViewId;
use shell_contract::runtime::PathName;

use super::dialog::{BoxOver, Parent};
use super::state::{App, CopyMark, SlideSession};
use crate::host::path_string;

/// `fileRecoveryId(filePath)`
pub fn file_recovery_id(path: &str) -> RecoveryIdentity {
    RecoveryIdentity::File(RecoveryFileIdentity {
        kind: "slides".to_owned(),
        file_path: path.to_owned(),
    })
}

/// What `recoveredOpenBytes` answers
pub struct RecoveredOpen {
    pub bytes: Vec<u8>,
    pub recovered: bool,
}

/// a clock a probe case fixes (`hosts.d1.slides.clock`, ms since the epoch); 0 is the real one
#[cfg(any(test, feature = "probe"))]
pub static FIXED_CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `Date.now()`
pub fn now_ms() -> f64 {
    #[cfg(any(test, feature = "probe"))]
    {
        let fixed = FIXED_CLOCK.load(std::sync::atomic::Ordering::Relaxed);
        if fixed != 0 {
            return fixed as f64;
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

impl App {
    /// The shared recovery store over the process-wide state
    pub fn with_recovery_store<R>(&self, f: impl FnOnce(&RecoveryStore<'_>) -> R) -> R {
        let shared = self.recovery_state.lock().unwrap();
        let user_data = path_string(self.services.paths.path(PathName::UserData));
        let user_data_dir = move || user_data.clone();
        let uuid = || self.shell.random_uuid();
        let store = create_recovery_store(
            RecoveryStoreOptions {
                user_data_dir: &user_data_dir,
                now_ms: &now_ms,
                random_uuid: &uuid,
            },
            &shared,
        );
        f(&store)
    }

    /// `copyNotOpened(identity, result)`: the copy waits for the next launch
    /// list or open as "later"
    fn copy_not_opened(&self, identity: &RecoveryIdentity) {
        self.with_recovery_store(|store| {
            let _ = store.set_copy_state(identity, RecoveryCopyState::Later);
        });
    }

    /// `recoveredOpenBytes(wcId, path, original, password)`: the bytes to
    /// open for `path`, the original or its recovery copy
    pub fn recovered_open_bytes(
        &self,
        view: ViewId,
        path: &str,
        original: Vec<u8>,
        password: Option<&str>,
    ) -> Result<RecoveredOpen, shell_contract::IpcError> {
        if self.shell.is_headless() {
            return Ok(RecoveredOpen {
                bytes: original,
                recovered: false,
            });
        }
        let identity = file_recovery_id(path);
        let forced = {
            let mut state = self.state.borrow_mut();
            let matches = state.recovery_open_decisions.get(&view).is_some_and(|request| {
                matches!((std::path::absolute(&request.file_path), std::path::absolute(path)), (Ok(left), Ok(right)) if left == right)
            });
            if matches {
                state.recovery_open_decisions.remove(&view)
            } else {
                None
            }
        };
        let answer = match forced {
            Some(request) => match request.decision {
                main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Original => None,
                main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Restore => {
                    self.with_recovery_store(|store| {
                        let copy = store.read_copy(&identity);
                        if copy.is_some() { let _ = store.mark_copy_loaded(&identity); }
                        copy
                    })
                }
            },
            None => {
                let parent = self.shell.dialog_parent().map(Parent);
                let lang = self.lang();
                self.with_recovery_store(|store| {
                    match resolve_recovery_on_open(&ResolveRecoveryOnOpenOptions {
                        store,
                        identity: &identity,
                        dialog: &BoxOver(&*self.dialogs),
                        parent: parent.as_ref(),
                        lang: lang.as_str(),
                        document_name: None,
                    }) {
                        Ok(RecoveryOpenResult::Restore { bytes, .. }) => Some(bytes),
                        _ => None,
                    }
                })
            }
        };
        let copy = match answer {
            Some(bytes) => bytes,
            _ => {
                return Ok(RecoveredOpen {
                    bytes: original,
                    recovered: false,
                });
            }
        };
        // an encrypted deck's copy is encrypted with the same password (a
        // copy the current password cannot open is left alone, not opened)
        if self
            .engine
            .is_encrypted_ooxml(&copy)
            .map_err(|error| crate::value::thrown(error.0))?
        {
            let Some(password) = password else {
                self.copy_not_opened(&identity);
                return Ok(RecoveredOpen {
                    bytes: original,
                    recovered: false,
                });
            };
            return Ok(match self.engine.decrypt_office_file(&copy, password) {
                Ok(bytes) => RecoveredOpen {
                    bytes,
                    recovered: true,
                },
                Err(_) => {
                    self.copy_not_opened(&identity);
                    RecoveredOpen {
                        bytes: original,
                        recovered: false,
                    }
                }
            });
        }
        Ok(RecoveredOpen {
            bytes: copy,
            recovered: true,
        })
    }

    /// `deleteSessionCopies(session, reason)`: every copy the session's
    /// document has (its file slot and its untitled slot)
    pub fn delete_session_copies(&self, s: &SlideSession, reason: RecoveryDeleteReason) {
        let mut refused = false;
        self.with_recovery_store(|store| {
            let mut delete = |identity: &RecoveryIdentity| {
                refused |= matches!(
                    store.delete_copy(identity, reason),
                    Ok(RecoveryDeleteResult::Refused)
                );
                if let Ok(key) = store.copy_path(identity) {
                    self.backoff.borrow_mut().remove(&key);
                }
            };
            if !s.path().is_empty() {
                delete(&file_recovery_id(s.path()));
            }
            if let Some(identity) = &s.recovery_id {
                delete(identity);
            }
        });
        if !refused {
            s.copy_mark.set(None);
        }
    }

    /// `recoveryCopiesSaved(session, previousPath)`: a save of the session
    /// succeeded (to `session.path`)
    pub fn recovery_copies_saved(&self, s: &mut SlideSession, previous_path: &str) {
        if !previous_path.is_empty() && previous_path != s.path() {
            self.with_recovery_store(|store| {
                let _ = store.delete_copy(
                    &file_recovery_id(previous_path),
                    RecoveryDeleteReason::Saved,
                );
            });
        }
        self.delete_session_copies(s, RecoveryDeleteReason::Saved);
        if !s.path().is_empty() {
            s.recovery_id = None;
        }
    }

    /// `releaseSlidesRecovery(webContentsId)`: a view is going away; a deck
    /// no other view shows that closes clean drops the copy it wrote
    pub fn release_recovery(&self, view: ViewId) {
        let state = self.state.borrow();
        let Some(slot) = state.slot_of(view) else {
            return;
        };
        let Some(session) = state.slot(slot) else {
            return;
        };
        match self.session_dirty(session) {
            Ok(false) => {}
            Ok(true) => return,
            Err(error) => {
                eprintln!("[slides] recovery cleanup dirty check failed: {}", error.0);
                return;
            }
        }
        if state.attached_ids(slot).iter().any(|other| *other != view) {
            return;
        }
        if session.recovery_id.is_none() {
            return;
        }
        self.delete_session_copies(session, RecoveryDeleteReason::ClosedClean);
    }

    // ---- the recent list ----

    fn recent_path(&self) -> String {
        path_string(
            self.services
                .paths
                .path(PathName::UserData)
                .join("slides-recent.json"),
        )
    }

    /// `readRecent()`: the stored list without the files that are gone
    pub fn read_recent(&self) -> Vec<String> {
        let Ok(raw) = std::fs::read_to_string(self.recent_path()) else {
            return Vec::new();
        };
        let Ok(JsonValue::Array(items)) = json_parse(&raw) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        for item in &items {
            // `existsSync` of a non-string throws, and the catch answers []
            let JsonValue::String(path) = item else {
                return Vec::new();
            };
            paths.push(path.clone());
        }
        paths.retain(|p| Path::new(p).exists());
        paths
    }

    /// `pushRecent(path)`: a headless export is not a document the user opened
    pub fn push_recent(&self, path: &str) {
        if self.shell.is_headless() {
            return;
        }
        super::recent::push_unowned(self.recent_path(), path.to_owned());
    }
}

// ---- the 30 second recovery copy pass ----

/// `setInterval(runSlidesRecoveryPass, 30_000)`
const RECOVERY_TICK: std::time::Duration = std::time::Duration::from_secs(30);
/// `AUTOSAVE_BACKOFF_TICKS`
const AUTOSAVE_BACKOFF_TICKS: u32 = 10;

/// `runSlidesRecoveryPass(ignoreBackoff)`: one pass over every open deck;
/// with `ignore_backoff` (the OS is ending the session) decks in backoff
/// are tried too. Called by the shell on the core thread.
pub fn run_recovery_pass(ignore_backoff: bool) {
    let app = super::APPS.with(|apps| apps.borrow().last().cloned());
    if let Some(app) = app
        && let Err(error) = app.run_recovery_pass(ignore_backoff)
    {
        eprintln!("[slides] recovery copy failed: {error}");
    }
}

impl App {
    /// `setTimeout(job, delay)`: a thread that only sleeps and posts; `job`
    /// finds the app again on the core thread
    pub fn after(&self, delay: std::time::Duration, job: impl FnOnce(&App) + Send + 'static) {
        let index = self.registry_index;
        let services = self.services.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            super::post_to(&services, index, job);
        });
    }

    /// Starts the recovery interval
    pub fn schedule_recovery_tick(&self) {
        self.after(RECOVERY_TICK, |app| {
            if let Err(error) = app.run_recovery_pass(false) {
                eprintln!("[slides] recovery copy failed: {error}");
            }
            app.schedule_recovery_tick();
        });
    }

    fn content_mark(&self, s: &SlideSession) -> Result<CopyMark, String> {
        let top = |stack: &[super::engine::Snapshot]| {
            stack
                .last()
                .map_or(0, |snap| std::rc::Rc::as_ptr(&snap.0) as *const () as usize)
        };
        Ok(CopyMark {
            op_seq: s.core.op_seq.unwrap_or(0),
            meta_rev: s.core.meta_rev.unwrap_or(0),
            meta_dirty: s.core.meta_dirty,
            undo_len: s.core.undo_stack.len(),
            undo_top: top(&s.core.undo_stack),
            redo_len: s.core.redo_stack.len(),
            redo_top: top(&s.core.redo_stack),
            slide_count: self
                .engine
                .slide_paths(s.deck())
                .map_err(|error| error.0)?
                .len(),
        })
    }

    /// `recoveryIdentityOf(session)`: its file's slot, else its own
    /// untitled slot (made on first use)
    pub(super) fn recovery_identity_of(&self, s: &mut SlideSession) -> RecoveryIdentity {
        if !s.path().is_empty() {
            return file_recovery_id(s.path());
        }
        s.recovery_id
            .get_or_insert_with(|| {
                RecoveryIdentity::Untitled(RecoveryUntitledIdentity {
                    kind: "slides".to_owned(),
                    untitled_id: new_untitled_recovery_id(&|| self.shell.random_uuid()),
                    ext: ".pptx".to_owned(),
                })
            })
            .clone()
    }

    /// `untitledRecoveryTitle(session)`: the launch list's name for an
    /// untitled deck
    fn untitled_recovery_title(&self, s: &SlideSession) -> String {
        let named = s
            .first_save_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .map(|n| {
                let base = jscompat::node_path::host::basename(n, None);
                match base.to_lowercase().strip_suffix(".pptx") {
                    Some(stem) => base[..stem.len()].to_owned(),
                    None => base,
                }
            })
            .unwrap_or_default();
        if named.is_empty() {
            self.tm("untitledDeck", &[])
        } else {
            named
        }
    }

    /// `writeSessionCopy(session, ignoreBackoff)`
    pub fn write_session_copy(&self, slot: usize, ignore_backoff: bool) -> Result<(), String> {
        let mut state = self.state.borrow_mut();
        let Some(s) = state.slot_mut(slot) else {
            return Err("The presentation session was closed".into());
        };
        let mark = self.content_mark(s)?;
        // the deck holds an AI run's unaccepted edits: no copy until it is reverted or accepted
        if s.core.ai_run {
            return Err("The presentation has unaccepted AI changes".into());
        }
        let identity = self.recovery_identity_of(s);
        let title = matches!(identity, RecoveryIdentity::Untitled(_))
            .then(|| self.untitled_recovery_title(s));
        let deck = s.deck();
        let engine = self.engine.clone();
        let password = s.password.clone();
        self.with_recovery_store(|store| {
            if store
                .is_discarded(&identity)
                .map_err(|error| error.message)?
            {
                return Err("The presentation recovery copy was discarded".into());
            }
            let key = store.copy_path(&identity).map_err(|error| error.message)?;
            let skip = self.backoff.borrow().get(&key).copied().unwrap_or(0);
            if skip > 0 && !ignore_backoff {
                self.backoff.borrow_mut().insert(key, skip - 1);
                return Ok(());
            }
            if s.copy_mark.get() == Some(mark) && matches!(store.copy_info(&identity), Ok(Some(_)))
            {
                return Ok(());
            }
            // the ticket comes before serializing: a save or Don't Save that
            // lands meanwhile wins
            let ticket = store
                .begin_write(&identity)
                .map_err(|error| error.message)?;
            let options = RecoveryWriteOptions {
                ticket: Some(ticket),
                title,
                ..RecoveryWriteOptions::default()
            };
            let node_error = |message: String| NodeError {
                code: None,
                message,
            };
            let result = match &password {
                None => store.write_copy_streamed(
                    &identity,
                    &mut |tmp| engine.save_to_file(deck, tmp).map_err(|e| node_error(e.0)),
                    &options,
                ),
                Some(password) => engine
                    .save_bytes(deck)
                    .and_then(|bytes| engine.encrypt_ooxml(&bytes, password))
                    .map_err(|e| node_error(e.0))
                    .and_then(|bytes| store.write_copy(&identity, &bytes, &options)),
            };
            match result {
                Ok(RecoveryWriteResult::Written) => {
                    self.backoff.borrow_mut().remove(&key);
                    s.copy_mark.set(Some(mark));
                    Ok(())
                }
                Ok(RecoveryWriteResult::Superseded) => {
                    Err("The presentation recovery copy was superseded".into())
                }
                Err(error) => {
                    // a run that began while the copy was starting is not a failing slot
                    if !s.core.ai_run {
                        self.backoff
                            .borrow_mut()
                            .insert(key, AUTOSAVE_BACKOFF_TICKS);
                    }
                    Err(error.message)
                }
            }
        })
    }

    /// `writeDirtyCopies(ignoreBackoff)`
    pub fn run_recovery_pass(&self, ignore_backoff: bool) -> Result<(), String> {
        let slots = self.state.borrow().live_slots();
        let mut first_failure = None;
        for slot in slots {
            let (dirty, master_edit) = {
                let state = self.state.borrow();
                match state.slot(slot) {
                    Some(s) => match self.session_dirty(s) {
                        Ok(dirty) => (dirty, s.master_edit.is_some()),
                        Err(error) => {
                            if first_failure.is_none() {
                                first_failure = Some(error.0);
                            }
                            continue;
                        }
                    },
                    None => (false, false),
                }
            };
            if !dirty {
                continue;
            }
            if master_edit {
                if ignore_backoff && first_failure.is_none() {
                    first_failure = Some("The presentation is editing its master slide".to_owned());
                }
                continue;
            }
            if let Err(error) = self.write_session_copy(slot, ignore_backoff)
                && first_failure.is_none()
            {
                first_failure = Some(error);
            }
        }
        first_failure.map_or(Ok(()), Err)
    }
}

#[path = "recovery_open.rs"]
mod opening;
pub(super) use opening::{
    PreparedRecovery, RecoveryApplying, abandon as abandon_recovery, consume as consume_prepared,
    initialized as recovery_initialized,
};
