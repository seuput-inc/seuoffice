//! The slides session model: `session-state.ts`'s `sessions` map, the module
//! level variables of `slides-main.ts` and `presenter-show.ts`, as one value
//! the handlers share (`Rc<RefCell<AppState>>`, core thread only).
//!
//! A deck session is shared by every view attached to it (a second window on
//! the same file, a presenter's audience window), so sessions sit in slots
//! and a view names its slot. Slots are never reused within a run, so a
//! deferred flush that finds its slot empty or replaced simply does nothing.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use jscompat::JsonValue;
use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
use main_core::packages::electron_utils::save_conflict::SaveDiskState;
use main_core::slides::paste_cascade::PasteCascade;
use main_core::slides::seed_queue::SeedQueue;
use main_core::slides::session_state::{Deck, PartPath, Session, SessionIds};
use shell_contract::hosts::{ClipboardHost, ContentsHost, DialogsHost, OsShellHost};
use shell_contract::runtime::Services;
use shell_contract::{IpcValue, ViewId};

use super::engine::{DeckId, SlidesEngineHost, SlidesShellHost, Snapshot};
use crate::host::{LocaleHost, RecoveryStateHost, Shared};
use crate::value::field;

/// A render slide: the render tree the renderer draws, opaque here but for
/// the `partPath` key the deck broadcast coalesces on
#[derive(Clone, Debug, PartialEq)]
pub struct RenderSlide(pub IpcValue);

impl PartPath for RenderSlide {
    fn part_path(&self) -> Option<&str> {
        crate::value::as_str(field(&self.0, "partPath"))
    }
}

/// The live deck of a session: a deck in the engine, released with the
/// session
pub struct DeckHandle {
    pub id: DeckId,
    engine: SlidesEngineHost,
}

impl DeckHandle {
    pub fn new(id: DeckId, engine: SlidesEngineHost) -> Self {
        DeckHandle { id, engine }
    }
}

/// Holds a new binding until the prepared session can replace the old one.
pub struct StagedBinding {
    engine: SlidesEngineHost,
    deck: DeckId,
    owner: super::engine::RenderOwner,
    armed: bool,
}

impl StagedBinding {
    pub fn new(
        engine: SlidesEngineHost,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
    ) -> Result<Self, super::engine::EngineError> {
        engine.bind_view(deck, owner)?;
        Ok(Self {
            engine,
            deck,
            owner: owner.clone(),
            armed: true,
        })
    }

    pub fn commit(
        mut self,
        previous: Option<(DeckId, super::engine::RenderOwner)>,
    ) -> Result<(), super::engine::EngineError> {
        if let Some((old, previous_owner)) = previous.filter(|(old, _)| *old != self.deck) {
            let same = previous_owner
                .context
                .renderer()
                .zip(self.owner.context.renderer())
                .is_some_and(|(a, b)| a.same_generation(b));
            if same {
                self.engine.replace_view(old, self.deck, &self.owner)?;
            } else {
                self.engine.unbind_view(old, &previous_owner)?;
            }
        }
        self.armed = false;
        Ok(())
    }
}

impl Drop for StagedBinding {
    fn drop(&mut self) {
        if self.armed {
            if let Err(error) = self.engine.unbind_view(self.deck, &self.owner) {
                eprintln!("[slides] staged binding cleanup failed: {}", error.0);
            }
        }
    }
}

impl From<super::engine::EngineError> for main_core::slides::ai_ipc::IpcError {
    fn from(error: super::engine::EngineError) -> Self {
        Self {
            code: None,
            message: error.0,
        }
    }
}

impl Deck<Snapshot> for DeckHandle {
    type Error = super::engine::EngineError;
    fn take_snapshot(&self, meta_dirty: bool) -> Result<Snapshot, Self::Error> {
        self.engine.snapshot(self.id, meta_dirty)
    }

    fn restore_snapshot(&mut self, fresh: Snapshot) -> Result<bool, Self::Error> {
        self.engine.restore(self.id, &fresh)
    }
}

impl Drop for DeckHandle {
    fn drop(&mut self) {
        self.engine.release(self.id)
    }
}

/// `session-state.ts`'s `Session` fields that main-core's [`Session`] holds
pub type CoreSession = Session<DeckHandle, Snapshot, IpcValue, RenderSlide>;

/// The part open in master view (`masterEdit`): the part path; the engine
/// keeps the live master slide the ops are seeded with
#[derive(Clone, Debug, PartialEq)]
pub struct MasterEdit {
    pub part_path: String,
}

/// `contentMark(session)`: what a recovery copy was taken of. Equal marks
/// mean the copy on disk already holds the deck.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CopyMark {
    pub op_seq: u64,
    pub meta_rev: u64,
    pub meta_dirty: bool,
    pub undo_len: usize,
    pub undo_top: usize,
    pub redo_len: usize,
    pub redo_top: usize,
    pub slide_count: usize,
}

/// A close waiting on the renderer: the view, the ticket its timeout finds it
/// by, and what to do with the answer
pub type CloseWaiter = (ViewId, u64, Box<dyn FnOnce(bool)>);
/// A Save As waiting on the renderer's `slides:save-as`: the ticket its
/// timeout finds it by, and what to do with what it came to
pub type SaveAsWaiter = (u64, Box<dyn FnOnce(bool)>);
/// `Session` of `session-state.ts`
pub struct SlideSession {
    /// fixed for the life of this session object (`SlidesDocument::id`)
    pub id: u64,
    pub core: CoreSession,
    pub ai_review: main_core::slides::ai_proposal::AiReview<Snapshot>,
    pub fit_width_px: f64,
    /// the disk snapshot of `path` this deck last read or wrote
    pub disk_state: Option<SaveDiskState>,
    /// the open password of a deck opened from an encrypted file
    pub password: Option<String>,
    /// an untitled copy's first save asks where
    pub ask_path_on_first_save: bool,
    /// the file name that first save suggests
    pub first_save_name: Option<String>,
    /// the crash-recovery copy of a deck without a path
    pub recovery_id: Option<RecoveryIdentity>,
    pub transform_preview: bool,
    pub preview_redo: Option<Vec<Snapshot>>,
    pub master_edit: Option<MasterEdit>,
    /// what the last recovery copy of this deck was taken of (`copyMarks`)
    pub copy_mark: Cell<Option<CopyMark>>,
}

impl SlideSession {
    pub fn new(path: &str, deck: DeckHandle, fit_width_px: f64) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        SlideSession {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            core: Session::new(path, deck),
            ai_review: main_core::slides::ai_proposal::AiReview::new(),
            fit_width_px,
            disk_state: None,
            password: None,
            ask_path_on_first_save: false,
            first_save_name: None,
            recovery_id: None,
            transform_preview: false,
            preview_redo: None,
            master_edit: None,
            copy_mark: Cell::new(None),
        }
    }

    pub fn deck(&self) -> DeckId {
        self.core.deck.id
    }

    pub fn path(&self) -> &str {
        &self.core.path
    }
}

/// `slideClipboard`: slides copied from any deck of this process
#[derive(Clone, Debug, PartialEq)]
pub struct SlideClipboard {
    pub bundles: Vec<IpcValue>,
    pub pngs: Option<Vec<IpcValue>>,
}

/// `lastSlidePaste` of one view
#[derive(Clone, Debug, PartialEq)]
pub struct LastSlidePaste {
    pub after_index: f64,
    pub undo_len: usize,
    pub changed: Vec<f64>,
}

/// `elementClipboard`: elements copied from any deck of this process
pub struct ElementClipboard {
    pub items: Vec<IpcValue>,
    pub cascade: PasteCascade,
    pub token: String,
    pub sender_id: f64,
}

/// A pending OS question or a recording of the narration microphone; the
/// 4 hour bound is a deadline checked when asked
pub struct NarrationMicrophone {
    /// `None` while the OS question is pending
    pub deadline: Option<Instant>,
    /// distinguishes a newer recording from the one a late answer belongs to
    pub ticket: u64,
}

/// `PresenterShow`
pub struct PresenterShow {
    pub audience: Option<(ViewId, shell_contract::WindowId)>,
    pub audience_owner: Option<super::engine::RenderOwner>,
    pub last_sync: Option<IpcValue>,
    pub owner: super::engine::RenderOwner,
    pub ticket: u64,
    pub starting: bool,
    pub start_waiters: Vec<shell_contract::ipc::Responder>,
}

/// The parts of [`AppState`] a handler holds next to a session
pub struct StateRest<'a> {
    pub slide_clipboard: &'a mut Option<SlideClipboard>,
    pub last_slide_paste: &'a mut HashMap<ViewId, LastSlidePaste>,
    pub element_clipboard: &'a mut Option<ElementClipboard>,
}

pub(super) struct PendingRecoverySeed {
    pub ticket: u64,
    pub target: shell_contract::ipc::RecoveryOpenTarget,
    pub snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    pub done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    pub subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
}

struct Attachment {
    slot: usize,
    owner: super::engine::RenderOwner,
    save_lifetime: shell_contract::runtime::RendererGeneration,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.save_lifetime.revoke();
    }
}

pub(super) struct RetiredBinding {
    _attachment: Option<Attachment>,
    _session: Option<SlideSession>,
}

/// All of the module state the slides handlers share
#[derive(Default)]
pub struct AppState {
    slots: Vec<Option<SlideSession>>,
    attach: HashMap<ViewId, Attachment>,
    /// `viewerWcIds`
    pub viewers: HashSet<ViewId>,
    pub ids: SessionIds,
    pub ai_window_lives: main_core::slides::ai_proposal::WindowLives,
    /// `pendingOpenPath`
    pub pending_open_path: Option<String>,
    pub(super) owned_seeds: HashMap<ViewId, super::owned_seed::OwnedSeed>,
    pub(super) saved_loads: HashMap<ViewId, super::saved_load::SavedLoad>,
    pub(super) saved_actor_identities: HashMap<ViewId, Rc<()>>,
    pub(super) native_actor_owners: HashMap<ViewId, shell_contract::ipc::NativeBrowserOwner>,
    pub(super) headless_targets: HashMap<ViewId, shell_contract::ipc::NativeInvocationDocument>,
    /// `pendingByWc`
    pub pending_by_view: HashMap<ViewId, String>,
    pub active_view: Option<ViewId>,
    pub recovery_open_decisions: HashMap<
        ViewId,
        main_core::packages::electron_utils::recovery_contract::RecoveryOpenFileRequest,
    >,
    pub(super) pending_recovery_seeds: HashMap<ViewId, PendingRecoverySeed>,
    pub(super) recovery_seeds: HashMap<
        ViewId,
        (
            shell_contract::ipc::RecoveryOpenTarget,
            Option<shell_contract::ipc::RecoverySeedReceipt>,
            super::recovery::PreparedRecovery,
        ),
    >,
    pub(super) recovery_applying: HashMap<ViewId, super::recovery::RecoveryApplying>,
    pub slide_clipboard: Option<SlideClipboard>,
    pub last_slide_paste: HashMap<ViewId, LastSlidePaste>,
    pub element_clipboard: Option<ElementClipboard>,
    /// `issuedCloudPages`
    pub issued_cloud_pages: HashSet<String>,
    /// `closeSaveWaiters`: the continuations of closes that wait for the
    /// renderer's `slides:close-save-result`, with the ticket their timeout
    /// finds them by; the last of a view is the one the map would hold
    pub close_waiters: Vec<CloseWaiter>,
    /// The Save As the menu asked a view for (`requestSlidesSave('saveAs')`):
    /// the continuation waits for the renderer's `slides:save-as`, with the
    /// ticket its timeout finds it by; one per view
    pub save_as_waiters: HashMap<ViewId, SaveAsWaiter>,
    pub(super) owned_saves: HashMap<String, super::owned_save::PendingSave>,
    pub(super) owned_writes: HashMap<String, super::owned_save::PendingWrite>,
    pub(super) generated_publications:
        HashMap<String, super::export::owned::PendingGeneratedPublication>,
    pub(super) owned_closes: HashMap<String, super::owned_close::PendingClose>,
    /// `autoSavePrefByWc`
    pub auto_save_pref: HashMap<ViewId, bool>,
    pub narration: HashMap<ViewId, NarrationMicrophone>,
    pub next_ticket: u64,
    /// `covers` of `template-cover.ts`, answers kept per (id, language)
    pub covers: HashMap<String, Option<IpcValue>>,
    pub seeds: SeedQueue,
    /// `presenterShows`, by presenter view
    pub presenter_shows: HashMap<ViewId, PresenterShow>,
    /// `audiencePresenter`
    pub audience_presenter: HashMap<ViewId, ViewId>,
    /// `pastedImageSeq`
    pub pasted_image_seq: u32,
    /// bumped by every show-fullscreen call: a delayed release only runs
    /// when no newer call came (the debounce of `showFsRelease`)
    pub show_fs_ticket: u64,
    pub show_fs_pending: Option<shell_contract::runtime::CancelToken>,
}

impl AppState {
    pub(super) fn reserve_saved_actor(&mut self, view: ViewId) -> Result<(), String> {
        if self.saved_actor_identities.contains_key(&view) {
            return Err("The saved presentation actor already exists".into());
        }
        self.saved_actor_identities
            .try_reserve(1)
            .map_err(|_| "The saved presentation actor could not be reserved".into())
    }

    pub(super) fn saved_actor(&self, view: ViewId) -> Option<Rc<()>> {
        self.saved_actor_identities.get(&view).cloned()
    }

    /// Call only after the actor and browser transition have been validated.
    pub(super) fn install_saved_actor(&mut self, view: ViewId, identity: Rc<()>) -> Option<Rc<()>> {
        self.saved_actor_identities.insert(view, identity)
    }

    pub(super) fn claim_saved_actor(&mut self, view: ViewId, identity: &Rc<()>) -> bool {
        if !self
            .saved_actor_identities
            .get(&view)
            .is_some_and(|current| Rc::ptr_eq(current, identity))
        {
            return false;
        }
        self.saved_actor_identities.remove(&view);
        true
    }

    /// `sessions.get(view)`
    pub fn session(&self, view: ViewId) -> Option<&SlideSession> {
        let slot = self.attach.get(&view)?.slot;
        self.slots.get(slot)?.as_ref()
    }

    pub fn session_mut(&mut self, view: ViewId) -> Option<&mut SlideSession> {
        let slot = self.attach.get(&view)?.slot;
        self.slots.get_mut(slot)?.as_mut()
    }

    pub fn has_session(&self, view: ViewId) -> bool {
        self.session(view).is_some()
    }

    /// The sender's session together with the clipboards a paste reads
    /// while it edits the session
    pub fn session_split(&mut self, view: ViewId) -> Option<(&mut SlideSession, StateRest<'_>)> {
        let slot = self.attach.get(&view)?.slot;
        let session = self.slots.get_mut(slot)?.as_mut()?;
        Some((
            session,
            StateRest {
                slide_clipboard: &mut self.slide_clipboard,
                last_slide_paste: &mut self.last_slide_paste,
                element_clipboard: &mut self.element_clipboard,
            },
        ))
    }

    /// The slot a view is attached to
    pub fn slot_of(&self, view: ViewId) -> Option<usize> {
        self.attach.get(&view).map(|attachment| attachment.slot)
    }

    pub fn owner_of(&self, view: ViewId) -> Option<&super::engine::RenderOwner> {
        self.attach.get(&view).map(|attachment| &attachment.owner)
    }

    pub(super) fn save_lifetime(
        &self,
        view: ViewId,
    ) -> Option<&shell_contract::runtime::RendererGeneration> {
        self.attach
            .get(&view)
            .map(|attachment| &attachment.save_lifetime)
    }

    pub fn binding_of(&self, view: ViewId) -> Option<(DeckId, super::engine::RenderOwner)> {
        Some((self.session(view)?.deck(), self.owner_of(view)?.clone()))
    }

    pub fn attached_owners(&self, slot: usize) -> Vec<super::engine::RenderOwner> {
        let mut owners: Vec<_> = self
            .attach
            .values()
            .filter(|attachment| attachment.slot == slot)
            .map(|attachment| attachment.owner.clone())
            .collect();
        owners.sort_by_key(|owner| owner.view.0);
        owners
    }

    pub fn slot(&self, slot: usize) -> Option<&SlideSession> {
        self.slots.get(slot)?.as_ref()
    }

    pub fn slot_mut(&mut self, slot: usize) -> Option<&mut SlideSession> {
        self.slots.get_mut(slot)?.as_mut()
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Install the first session of a newly authenticated editor document.
    pub(super) fn set_initial_session(
        &mut self,
        owner: super::engine::RenderOwner,
        session: SlideSession,
    ) -> Result<usize, String> {
        if self.attach.contains_key(&owner.view) {
            return Err("The editor already has a presentation".into());
        }
        self.slots.push(Some(session));
        let slot = self.slots.len() - 1;
        self.attach_initial_view(owner, slot)?;
        Ok(slot)
    }

    pub(super) fn reserve_open_slot(&mut self) -> usize {
        self.attach.reserve(1);
        self.slots.push(None);
        self.slots.len() - 1
    }

    pub(super) fn attach_retiring(
        &mut self,
        owner: super::engine::RenderOwner,
        slot: usize,
        replacement: Option<SlideSession>,
        successor: Option<shell_contract::ipc::PreparedDocumentOwner>,
        expected: Option<&shell_contract::runtime::RendererGeneration>,
        retirement: &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
    ) -> Result<(RetiredBinding, super::engine::RenderOwner), String> {
        match (self.attach.get(&owner.view), expected) {
            (Some(previous), Some(expected))
                if previous.save_lifetime.same_generation(expected) => {}
            (None, None) => {}
            _ => return Err("The presentation changed before its open transition".into()),
        }
        if slot >= self.slots.len() || (replacement.is_none() && self.slots[slot].is_none()) {
            return Err("The prepared presentation slot was released".into());
        }
        if expected.is_some() && successor.is_none() {
            return Err("The presentation successor was not prepared".into());
        }
        let owner = if let Some(expected) = expected {
            retirement.retire(expected)?;
            retirement.renew_owner(successor.ok_or("The presentation successor was released")?)?
        } else {
            owner
        };
        let lifetime = owner
            .context
            .document_lease()
            .ok_or("The presentation successor has no document lease")?
            .clone();
        if let Some(replacement) = replacement {
            self.slots[slot] = Some(replacement);
        }
        let previous = self.attach.insert(
            owner.view,
            Attachment {
                slot,
                owner: owner.clone(),
                save_lifetime: lifetime,
            },
        );
        let old_slot = previous.as_ref().map(|attachment| attachment.slot);
        let old_session = old_slot
            .filter(|old| {
                *old != slot
                    && !self
                        .attach
                        .values()
                        .any(|attachment| attachment.slot == *old)
            })
            .and_then(|old| self.slots.get_mut(old))
            .and_then(Option::take);
        Ok((
            RetiredBinding {
                _attachment: previous,
                _session: old_session,
            },
            owner,
        ))
    }

    pub(super) fn replace_shared_retiring<F: FnOnce(&mut SlideSession)>(
        &mut self,
        plan: &mut super::shared_open::SharedOpen,
        participants: &[super::engine::RenderOwner],
        new_slot: usize,
        replacement: &mut Option<SlideSession>,
        snapshot: &mut Option<Snapshot>,
        finalize: &mut Option<F>,
        retired: &mut Vec<RetiredBinding>,
        retired_session: &mut Option<SlideSession>,
        completed: &mut Vec<super::engine::RenderOwner>,
        retirement: &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
    ) -> Result<(), String> {
        let old_slot = plan
            .slot
            .ok_or("The shared presentation slot was not captured")?;
        let revision = plan
            .revision
            .as_ref()
            .ok_or("The shared presentation revision was not captured")?;
        let old = self
            .slot(old_slot)
            .ok_or("The shared presentation was released")?;
        if Some(old.id) != plan.session
            || Some(old.deck()) != plan.deck
            || old.path() != revision.path
            || old.core.op_seq != revision.op_seq
            || old.core.meta_rev != revision.meta_rev
            || self
                .attach
                .values()
                .filter(|attachment| attachment.slot == old_slot)
                .count()
                != participants.len()
            || participants.len() != plan.ids.len()
            || participants.len() != plan.successors.len()
            || plan.successors.iter().any(Option::is_none)
            || replacement.is_none()
            || snapshot.is_none()
            || finalize.is_none()
            || new_slot >= self.slots.len()
            || self.slots[new_slot].is_some()
        {
            return Err("The shared presentation changed before replacement".into());
        }
        for (index, participant) in participants.iter().enumerate() {
            let held = self
                .attach
                .get(&participant.view)
                .ok_or("An attached presentation was released")?;
            if participant.view != plan.ids[index]
                || held.slot != old_slot
                || !participant
                    .context
                    .document_lease()
                    .is_some_and(|document| document.same_generation(&held.save_lifetime))
                || !held
                    .owner
                    .context
                    .renderer()
                    .zip(participant.context.renderer())
                    .is_some_and(|(actual, captured)| actual.same_generation(captured))
            {
                return Err("An attached presentation changed before replacement".into());
            }
        }
        retirement
            .validate_owner_renewals_iter(plan.successors.iter().filter_map(Option::as_ref))?;
        for participant in participants {
            retirement.retire(
                participant
                    .context
                    .document_lease()
                    .ok_or("An attached document lease was released")?,
            )?;
        }
        let mut fresh = replacement
            .take()
            .ok_or("The generated presentation was released")?;
        let old = self
            .slot_mut(old_slot)
            .ok_or("The shared presentation was released")?;
        Session::carry_history_with_snapshot(
            &mut old.core,
            &mut fresh.core,
            snapshot
                .take()
                .ok_or("The presentation history was released")?,
        );
        if old.path().is_empty() {
            fresh.recovery_id = old.recovery_id.clone();
            fresh.ai_review = std::mem::take(&mut old.ai_review);
        }
        finalize
            .take()
            .ok_or("The generated presentation finalizer was released")?(&mut fresh);
        self.slots[new_slot] = Some(fresh);
        for successor in &mut plan.successors {
            let owner = retirement.renew_owner(
                successor
                    .take()
                    .ok_or("An attached document successor was released")?,
            )?;
            let lifetime = owner
                .context
                .document_lease()
                .ok_or("An attached successor has no document lease")?
                .clone();
            let previous = self.attach.insert(
                owner.view,
                Attachment {
                    slot: new_slot,
                    owner: owner.clone(),
                    save_lifetime: lifetime,
                },
            );
            if let Some(previous) = &previous {
                if let Some(show) = self.presenter_shows.get_mut(&owner.view)
                    && show
                        .owner
                        .context
                        .document_lease()
                        .is_some_and(|document| document.same_generation(&previous.save_lifetime))
                {
                    show.owner = owner.clone();
                }
                if let Some(presenter) = self.audience_presenter.get(&owner.view)
                    && let Some(show) = self.presenter_shows.get_mut(presenter)
                    && show
                        .audience_owner
                        .as_ref()
                        .and_then(|audience| audience.context.document_lease())
                        .is_some_and(|document| document.same_generation(&previous.save_lifetime))
                {
                    show.audience_owner = Some(owner.clone());
                }
            }
            retired.push(RetiredBinding {
                _attachment: previous,
                _session: None,
            });
            completed.push(owner);
        }
        *retired_session = self.slots[old_slot].take();
        Ok(())
    }

    /// Join an existing deck only when this editor has no previous document.
    pub(super) fn attach_initial_view(
        &mut self,
        owner: super::engine::RenderOwner,
        slot: usize,
    ) -> Result<(), String> {
        if self.attach.contains_key(&owner.view) || self.slot(slot).is_none() {
            return Err("The editor attachment is no longer initial".into());
        }
        let lifetime = owner
            .context
            .document_lease()
            .cloned()
            .unwrap_or_else(shell_contract::runtime::RendererGeneration::new);
        self.attach.insert(
            owner.view,
            Attachment {
                slot,
                owner,
                save_lifetime: lifetime,
            },
        );
        Ok(())
    }

    /// Detach under the actor transition; defer lease revocation and deck drop.
    pub(super) fn detach_view_retiring(&mut self, view: ViewId) -> RetiredBinding {
        let attachment = self.attach.remove(&view);
        let session = attachment.as_ref().and_then(|attachment| {
            if self
                .attach
                .values()
                .any(|other| other.slot == attachment.slot)
            {
                return None;
            }
            self.slots.get_mut(attachment.slot).and_then(Option::take)
        });
        RetiredBinding {
            _attachment: attachment,
            _session: session,
        }
    }

    /// `sessions.delete(view)`
    pub fn detach_view(&mut self, view: ViewId) {
        if let Some(attachment) = self.attach.remove(&view) {
            self.drop_if_unattached(attachment.slot);
        }
    }

    fn drop_if_unattached(&mut self, slot: usize) {
        if !self
            .attach
            .values()
            .any(|attachment| attachment.slot == slot)
            && let Some(entry) = self.slots.get_mut(slot)
        {
            *entry = None;
        }
    }

    /// `attachedIds(session)`, in the order the views attached... views sort
    /// by id so the order is stable
    pub fn attached_ids(&self, slot: usize) -> Vec<ViewId> {
        let mut ids: Vec<ViewId> = self
            .attach
            .iter()
            .filter(|(_, attachment)| attachment.slot == slot)
            .map(|(view, _)| *view)
            .collect();
        ids.sort_by_key(|view| view.0);
        ids
    }

    /// `editorAttachedIds(session)`
    pub fn editor_attached_ids(&self, slot: usize) -> Vec<ViewId> {
        self.attached_ids(slot)
            .into_iter()
            .filter(|view| !self.viewers.contains(view))
            .collect()
    }

    /// The slots that hold a session, once each
    pub fn live_slots(&self) -> Vec<usize> {
        (0..self.slots.len())
            .filter(|slot| self.slots[*slot].is_some())
            .collect()
    }
}

/// Everything a handler needs, cloned into each closure: the shared state
/// and the hosts
#[derive(Clone)]
pub struct App {
    pub state: Rc<RefCell<AppState>>,
    /// where [`super::with_app`] finds this app on the core thread
    pub registry_index: usize,
    pub services: Services,
    pub engine: SlidesEngineHost,
    pub shell: SlidesShellHost,
    pub dialogs: DialogsHost,
    pub os_shell: OsShellHost,
    pub clipboard: ClipboardHost,
    pub contents: ContentsHost,
    pub locale: LocaleHost,
    pub shared: Arc<Shared>,
    pub recovery_state: RecoveryStateHost,
    /// the attachment text cache, read on workers
    pub text_cache: super::files::SharedTextCache,
    /// what the batches provide to each other; read at call time
    pub provided: Rc<shell_contract::provided::Provided>,
    /// `autosaveBackoff`: ticks to skip after a failed recovery copy, per copy slot
    pub backoff: Rc<RefCell<HashMap<String, u32>>>,
}

/// A JSON number, for `tm` parameters
pub fn json_number(value: f64) -> JsonValue {
    JsonValue::Number(value)
}
