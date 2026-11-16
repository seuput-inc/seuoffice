//! `apps/docs/src/main/byte-handoff.ts`: document bytes cross to the renderer
//! over the media protocol instead of an IPC reply. A one-shot token URL
//! hands the buffer to a single fetch.
//!
//! The module state (`pending`, `pendingBytes`) is a [`ByteHandoffs`] value.
//! The TypeScript gives each handoff a `setTimeout` of `HANDOFF_TTL_MS`; here
//! the caller owns the clock: a handoff carries the time it was made, and
//! [`ByteHandoffs::expire`] is what the timers' callbacks do when they fire.
//! `crypto.randomUUID()` is the `token` parameter.

use super::lazy_media_inline::LAZY_MEDIA_SCHEME;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

const HANDOFF_HOST: &str = "handoff";
/// an unclaimed handoff (renderer gone before its fetch) is dropped after this
pub const HANDOFF_TTL_MS: f64 = 60_000.0;
/// a burst past this many unclaimed handoffs drops the oldest one
pub const MAX_PENDING_HANDOFFS: usize = 8;
/// a burst past this many buffered bytes drops the oldest handoffs
pub const MAX_PENDING_HANDOFF_BYTES: u64 = 1024 * 1024 * 1024;

/// `bytes.length`
pub trait ByteLength {
    fn byte_length(&self) -> u64;
}

impl ByteLength for Vec<u8> {
    fn byte_length(&self) -> u64 {
        self.len() as u64
    }
}

/// Identity of one live document view, including its non-reusable lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MediaOwner {
    pub view: u32,
    pub generation: u64,
}

/// A capability captured on the core thread and invalidated on document teardown.
#[derive(Clone, Debug)]
pub struct MediaPermit {
    owner: MediaOwner,
    active: Arc<PermitLifetime>,
}

type LossCallback = Box<dyn FnOnce() + Send + 'static>;

struct LossListener {
    callback: Mutex<Option<LossCallback>>,
}
struct PermitGate {
    listeners: Vec<Arc<LossListener>>,
}
struct PermitLifetime {
    active: AtomicBool,
    gate: Mutex<PermitGate>,
}

impl std::fmt::Debug for PermitLifetime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermitLifetime")
            .field("active", &self.active.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// Retain this registration while its document loss must be observed.
pub struct MediaLossSubscription {
    lifetime: Weak<PermitLifetime>,
    listener: Weak<LossListener>,
}

impl Drop for MediaLossSubscription {
    fn drop(&mut self) {
        let Some(lifetime) = self.lifetime.upgrade() else {
            return;
        };
        let removed = {
            let mut gate = lifetime
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            gate.listeners
                .iter()
                .position(|listener| std::ptr::eq(Arc::as_ptr(listener), self.listener.as_ptr()))
                .map(|index| gate.listeners.remove(index))
        };
        // Dropping a captured callback may release another subscription.
        drop(removed);
    }
}

fn notify_loss(listeners: Vec<Arc<LossListener>>) {
    for listener in listeners {
        let callback = listener
            .callback
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(callback) = callback {
            callback();
        }
    }
}

impl MediaPermit {
    pub fn mint(view: u32) -> Result<Self, String> {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let generation = NEXT
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
            .map_err(|_| "Document media generation exhausted.".to_owned())?;
        Ok(Self {
            owner: MediaOwner { view, generation },
            active: Arc::new(PermitLifetime {
                active: AtomicBool::new(true),
                gate: Mutex::new(PermitGate {
                    listeners: Vec::new(),
                }),
            }),
        })
    }

    pub fn owner(&self) -> MediaOwner {
        self.owner
    }
    pub fn is_active(&self) -> bool {
        self.active.active.load(Ordering::SeqCst)
    }

    /// A short, non-reentrant final effect, serialized with document revocation.
    /// Never wait, enter UI/core, or call view-lease methods from this closure.
    pub fn with_active<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
        Self::with_active_all([Some(self)], effect)
    }

    /// Gate all captured document permits in one canonical order. Aliases are
    /// locked once, so an origin and destination in the same document are safe.
    /// The effect must obey the same non-reentrancy contract as with_active.
    pub fn with_active_all<const N: usize, T>(
        mut permits: [Option<&Self>; N],
        effect: impl FnOnce() -> T,
    ) -> Option<T> {
        permits.sort_unstable_by_key(|permit| {
            permit.map(|permit| permit.owner.generation).unwrap_or(0)
        });
        let mut guards: [Option<std::sync::MutexGuard<'_, PermitGate>>; N] =
            std::array::from_fn(|_| None);
        for (index, permit) in permits.iter().enumerate() {
            let Some(permit) = permit else { continue };
            if index > 0 && permits[index - 1].is_some_and(|previous| previous.same_session(permit))
            {
                continue;
            }
            guards[index] = Some(
                permit
                    .active
                    .gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
            if !permit.is_active() {
                return None;
            }
        }
        Some(effect())
    }

    /// Notify once on loss, including a subscription made after revocation.
    /// Callbacks run without permit gates or view-lease state locks.
    pub fn on_revoke(&self, callback: impl FnOnce() + Send + 'static) -> MediaLossSubscription {
        let mut callback: Option<LossCallback> = Some(Box::new(callback));
        let listener = {
            let mut gate = self
                .active
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.is_active() {
                let listener = Arc::new(LossListener {
                    callback: Mutex::new(callback.take()),
                });
                gate.listeners.push(listener.clone());
                Some(listener)
            } else {
                None
            }
        };
        if let Some(callback) = callback {
            callback();
        }
        MediaLossSubscription {
            lifetime: Arc::downgrade(&self.active),
            listener: listener.as_ref().map(Arc::downgrade).unwrap_or_default(),
        }
    }

    fn revoke_deferred(&self) -> Vec<Arc<LossListener>> {
        let mut gate = self
            .active
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !self.active.active.swap(false, Ordering::SeqCst) {
            return Vec::new();
        }
        std::mem::take(&mut gate.listeners)
    }

    pub fn revoke(&self) {
        notify_loss(self.revoke_deferred());
    }
    pub fn same_session(&self, other: &Self) -> bool {
        self.owner == other.owner && Arc::ptr_eq(&self.active, &other.active)
    }
}

/// Notifications are delivered only after the caller releases every enclosing gate.
#[derive(Default)]
pub struct DeferredMediaLoss(Option<Vec<Arc<LossListener>>>);
impl Drop for DeferredMediaLoss {
    fn drop(&mut self) {
        if let Some(listeners) = self.0.take() {
            notify_loss(listeners);
        }
    }
}

pub struct AdmittedMediaRetirement<'a> {
    permit: &'a MediaPermit,
    successor: &'a MediaPermit,
    gate: &'a mut PermitGate,
    notifications: &'a mut DeferredMediaLoss,
}

impl AdmittedMediaRetirement<'_> {
    fn retire(&mut self, expected: &MediaPermit) -> Result<(), String> {
        if !self.permit.same_session(expected)
            || !expected.is_active()
            || self.notifications.0.is_some()
        {
            return Err("The original media permit is no longer active.".into());
        }
        expected.active.active.store(false, Ordering::SeqCst);
        self.notifications.0 = Some(std::mem::take(&mut self.gate.listeners));
        Ok(())
    }
}

/// Prepare before entering any enclosing native authority gate. Captured
/// permits are canonicalized here; with_active never allocates or looks up a
/// current document. Construct on the execution thread (guard storage is !Send).
pub struct MediaPermitGatePlan<'a> {
    permits: Vec<&'a MediaPermit>,
    guards: Vec<std::sync::MutexGuard<'a, PermitGate>>,
}

impl<'a> MediaPermitGatePlan<'a> {
    pub fn new(permits: &[&'a MediaPermit]) -> Self {
        let mut permits = permits.to_vec();
        permits.sort_unstable_by_key(|permit| permit.owner.generation);
        permits.dedup_by(|left, right| left.same_session(right));
        let guards = Vec::with_capacity(permits.len());
        Self { permits, guards }
    }

    /// A short non-reentrant final effect, with the same contract as
    /// MediaPermit::with_active. Guard storage is empty again on every exit.
    pub fn with_active<T>(&mut self, effect: impl FnOnce() -> T) -> Option<T> {
        struct Held<'a, 'b>(&'b mut Vec<std::sync::MutexGuard<'a, PermitGate>>);
        impl Drop for Held<'_, '_> {
            fn drop(&mut self) {
                self.0.clear();
            }
        }
        let held = Held(&mut self.guards);
        for &permit in &self.permits {
            held.0.push(
                permit
                    .active
                    .gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
            if !permit.is_active() {
                return None;
            }
        }
        Some(effect())
    }

    pub fn with_active_retiring<T>(
        &mut self,
        old: &MediaPermit,
        successor: &MediaPermit,
        notifications: &mut DeferredMediaLoss,
        effect: impl FnOnce(&mut AdmittedMediaRetirement<'_>) -> T,
    ) -> Option<T> {
        let index = self
            .permits
            .iter()
            .position(|permit| permit.same_session(old))?;
        let next_index = self
            .permits
            .iter()
            .position(|permit| permit.same_session(successor))?;
        if index == next_index {
            return None;
        }
        struct Held<'a, 'b>(&'b mut Vec<std::sync::MutexGuard<'a, PermitGate>>);
        impl Drop for Held<'_, '_> {
            fn drop(&mut self) {
                self.0.clear();
            }
        }
        let held = Held(&mut self.guards);
        for &permit in &self.permits {
            held.0.push(
                permit
                    .active
                    .gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
            if !permit.is_active() {
                return None;
            }
        }
        let mut retirement = AdmittedMediaRetirement {
            permit: self.permits[index],
            successor: self.permits[next_index],
            gate: &mut held.0[index],
            notifications,
        };
        Some(effect(&mut retirement))
    }
}

#[derive(Clone, Debug)]
pub struct MediaViewLease {
    view: u32,
    state: std::sync::Arc<std::sync::Mutex<LeaseState>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaViewRetirement {
    Active,
    Retired,
    Replaced,
}

#[derive(Debug)]
struct LeaseState {
    live: bool,
    current: MediaPermit,
    pending: Option<MediaPermit>,
    retirement: MediaViewRetirement,
}

#[derive(Clone, Debug)]
pub struct PreparedMediaCandidate {
    state: Weak<Mutex<LeaseState>>,
    permit: MediaPermit,
}
impl PreparedMediaCandidate {
    pub fn permit(&self) -> &MediaPermit {
        &self.permit
    }
}

pub struct StagedMediaTransition<'a> {
    state: &'a mut LeaseState,
    old: &'a MediaPermit,
    successor: &'a MediaPermit,
}
impl StagedMediaTransition<'_> {
    pub fn commit(self, retirement: &mut AdmittedMediaRetirement<'_>) -> Result<(), String> {
        if !retirement.successor.same_session(self.successor) || !self.successor.is_active() {
            return Err("The staged media permit is no longer active.".into());
        }
        retirement.retire(self.old)?;
        self.state.pending = None;
        self.state.current = self.successor.clone();
        Ok(())
    }
}

pub struct PreparedMediaStage<'a> {
    state: &'a mut LeaseState,
    old: &'a MediaPermit,
    prior: Option<&'a MediaPermit>,
    candidate: &'a MediaPermit,
    retire_prior: bool,
}
impl PreparedMediaStage<'_> {
    pub fn retirement_target(&self) -> &MediaPermit {
        if self.retire_prior {
            self.prior.expect("active prior exists")
        } else {
            self.old
        }
    }

    pub fn commit(self, admitted: &mut AdmittedMediaRetirement<'_>) -> Result<(), String> {
        if !admitted.successor.same_session(self.candidate)
            || !self.candidate.is_active()
            || !admitted.permit.same_session(self.retirement_target())
        {
            return Err("The prepared media stage is no longer admitted.".into());
        }
        if self.retire_prior {
            admitted.retire(self.prior.expect("active prior exists"))?;
        }
        self.state.pending = Some(self.candidate.clone());
        Ok(())
    }
}

impl MediaViewLease {
    pub fn new(view: u32) -> Result<Self, String> {
        let current = MediaPermit::mint(view)?;
        Ok(Self {
            view,
            state: std::sync::Arc::new(std::sync::Mutex::new(LeaseState {
                live: true,
                current,
                pending: None,
                retirement: MediaViewRetirement::Active,
            })),
        })
    }

    pub fn view(&self) -> u32 {
        self.view
    }
    pub fn same_view(&self, other: &Self) -> bool {
        self.view == other.view && std::sync::Arc::ptr_eq(&self.state, &other.state)
    }

    pub fn retirement(&self) -> MediaViewRetirement {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retirement
    }

    pub fn mark_retired(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.retirement == MediaViewRetirement::Active {
            state.retirement = MediaViewRetirement::Retired;
        }
    }

    pub fn mark_replaced(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.retirement == MediaViewRetirement::Active {
            state.retirement = MediaViewRetirement::Replaced;
        }
    }

    pub fn current(&self) -> Option<MediaPermit> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        (state.live && state.current.is_active()).then(|| state.current.clone())
    }

    pub fn last_permit(&self) -> MediaPermit {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current
            .clone()
    }

    pub fn pending(&self) -> Option<MediaPermit> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .clone()
    }

    /// Allocates a candidate without changing this lease or revoking a pending document.
    pub fn prepare_document(&self) -> Result<PreparedMediaCandidate, String> {
        Ok(PreparedMediaCandidate {
            state: Arc::downgrade(&self.state),
            permit: MediaPermit::mint(self.view)?,
        })
    }

    pub fn with_prepared_stage<T>(
        &self,
        old: &MediaPermit,
        prior: Option<&MediaPermit>,
        prepared: &PreparedMediaCandidate,
        effect: impl FnOnce(PreparedMediaStage<'_>) -> T,
    ) -> Result<T, String> {
        if !Weak::ptr_eq(&prepared.state, &Arc::downgrade(&self.state)) {
            return Err("The prepared media candidate belongs to another view lifetime.".into());
        }
        let candidate = prepared.permit();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let same_prior = match (state.pending.as_ref(), prior) {
            (None, None) => true,
            (Some(current), Some(expected)) => current.same_session(expected),
            _ => false,
        };
        if !state.live
            || !state.current.same_session(old)
            || !same_prior
            || candidate.owner().view != self.view
            || candidate.owner().generation <= old.owner().generation
            || prior
                .is_some_and(|previous| previous.owner().generation >= candidate.owner().generation)
        {
            return Err("The captured media staging state was replaced.".into());
        }
        let retire_prior = prior.is_some_and(MediaPermit::is_active);
        Ok(effect(PreparedMediaStage {
            state: &mut state,
            old,
            prior,
            candidate,
            retire_prior,
        }))
    }
    pub fn stage_document(&self) -> Result<MediaPermit, String> {
        let (next, listeners) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.live {
                return Err("The document view is closed.".to_owned());
            }
            let next = MediaPermit::mint(self.view)?;
            let listeners = state
                .pending
                .replace(next.clone())
                .map(|previous| previous.revoke_deferred())
                .unwrap_or_default();
            (next, listeners)
        };
        notify_loss(listeners);
        Ok(next)
    }

    /// The closure may enter native admission but must not reenter this lease.
    pub fn with_staged_transition<T>(
        &self,
        old: &MediaPermit,
        successor: &MediaPermit,
        effect: impl FnOnce(StagedMediaTransition<'_>) -> T,
    ) -> Result<T, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.live
            || !state.current.same_session(old)
            || !state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.same_session(successor))
        {
            return Err("The captured media transition is no longer current.".into());
        }
        Ok(effect(StagedMediaTransition {
            state: &mut state,
            old,
            successor,
        }))
    }

    pub fn commit_document(&self, permit: &MediaPermit) -> Result<MediaPermit, String> {
        let (previous, listeners) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.live
                || !state
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.same_session(permit))
            {
                return Err("The document media session is closed.".to_owned());
            }
            // Staging always mints a newer generation. Match with_active_all's
            // old-to-new lock order while retaining both gates for the transition.
            let previous = state.current.clone();
            let mut previous_gate = previous
                .active
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let candidate = permit
                .active
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !permit.is_active() {
                return Err("The document media session is closed.".to_owned());
            }
            state.pending = None;
            state.current = permit.clone();
            let listeners = if previous.active.active.swap(false, Ordering::SeqCst) {
                std::mem::take(&mut previous_gate.listeners)
            } else {
                Vec::new()
            };
            drop(candidate);
            drop(previous_gate);
            (previous, listeners)
        };
        notify_loss(listeners);
        Ok(previous)
    }

    pub fn abort_document(&self, permit: &MediaPermit) {
        let listeners = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.same_session(permit))
            {
                state.pending = None;
                permit.revoke_deferred()
            } else {
                Vec::new()
            }
        };
        notify_loss(listeners);
    }

    pub fn revoke(&self) {
        let listeners = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.live = false;
            let mut listeners = state.current.revoke_deferred();
            if let Some(pending) = &state.pending {
                listeners.extend(pending.revoke_deferred());
            }
            listeners
        };
        notify_loss(listeners);
    }
}

struct Entry<B> {
    token: String,
    bytes: B,
    owner: Option<MediaOwner>,
    /// the id of this entry's timer (`entry.timer`)
    timer: u64,
}

struct Timer {
    id: u64,
    token: String,
    due_ms: f64,
}

/// The `pending` map (insertion-ordered), `pendingBytes`, and the timers of
/// the entries.
pub struct ByteHandoffs<B> {
    pending: Vec<Entry<B>>,
    pending_bytes: u64,
    timers: Vec<Timer>,
    next_timer: u64,
}

impl<B> Default for ByteHandoffs<B> {
    fn default() -> Self {
        ByteHandoffs {
            pending: Vec::new(),
            pending_bytes: 0,
            timers: Vec::new(),
            next_timer: 0,
        }
    }
}

impl<B: ByteLength> ByteHandoffs<B> {
    pub fn new() -> Self {
        Self::default()
    }

    /// release one handoff's timer, entry and bytes; the timer must go with the entry
    fn drop_pending(&mut self, token: &str) {
        let Some(at) = self.pending.iter().position(|entry| entry.token == token) else {
            return;
        };
        let entry = self.pending.remove(at);
        self.timers.retain(|timer| timer.id != entry.timer);
        self.pending_bytes -= entry.bytes.byte_length();
    }

    /// oldest-first eviction until `bytes` fits under both caps
    fn evict_for(&mut self, length: u64) {
        while self.pending.len() + 1 > MAX_PENDING_HANDOFFS
            || self.pending_bytes + length > MAX_PENDING_HANDOFF_BYTES
        {
            let Some(oldest) = self.pending.first() else {
                return;
            };
            let token = oldest.token.clone();
            self.drop_pending(&token);
        }
    }

    /// `handOffBytes(bytes)`: the URL the renderer fetches once. `now_ms` is
    /// the clock the timer starts at.
    pub fn hand_off_bytes(&mut self, bytes: B, token: String, now_ms: f64) -> String {
        self.evict_for(bytes.byte_length());
        let timer = self.next_timer;
        self.next_timer += 1;
        self.timers.push(Timer {
            id: timer,
            token: token.clone(),
            due_ms: now_ms + HANDOFF_TTL_MS,
        });
        self.pending_bytes += bytes.byte_length();
        let url = format!("{LAZY_MEDIA_SCHEME}://{HANDOFF_HOST}/{token}");
        // `pending.set(token, ...)` keeps the place of a token set before
        match self.pending.iter_mut().find(|entry| entry.token == token) {
            Some(entry) => {
                entry.bytes = bytes;
                entry.timer = timer;
                entry.owner = None;
            }
            None => self.pending.push(Entry {
                token,
                bytes,
                owner: None,
                timer,
            }),
        }
        url
    }

    /// The bytes behind a handoff URL, released on first take; `None` for any
    /// other URL.
    pub fn take_handoff(&mut self, url: &str) -> Option<B> {
        let prefix = format!("{LAZY_MEDIA_SCHEME}://{HANDOFF_HOST}/");
        let token = url.strip_prefix(prefix.as_str())?;
        let at = self
            .pending
            .iter()
            .position(|entry| entry.token == token && entry.owner.is_none())?;
        let entry = self.pending.remove(at);
        self.timers.retain(|timer| timer.id != entry.timer);
        self.pending_bytes -= entry.bytes.byte_length();
        Some(entry.bytes)
    }

    /// Issue a handoff bound to one document lifetime.
    pub fn hand_off_bytes_for(
        &mut self,
        bytes: B,
        token: String,
        now_ms: f64,
        owner: MediaOwner,
    ) -> Result<String, String> {
        if self.pending.iter().any(|entry| entry.token == token) {
            return Err("Duplicate byte handoff token.".to_owned());
        }
        let url = self.hand_off_bytes(bytes, token.clone(), now_ms);
        if let Some(entry) = self.pending.iter_mut().find(|entry| entry.token == token) {
            entry.owner = Some(owner);
        }
        Ok(url)
    }

    /// A failed cross-view request must not consume the owner's one-shot URL.
    pub fn take_handoff_for(&mut self, url: &str, owner: MediaOwner) -> Option<B> {
        let token = url.strip_prefix(&format!("{LAZY_MEDIA_SCHEME}://{HANDOFF_HOST}/"))?;
        let at = self
            .pending
            .iter()
            .position(|entry| entry.token == token && entry.owner == Some(owner))?;
        let entry = self.pending.remove(at);
        self.timers.retain(|timer| timer.id != entry.timer);
        self.pending_bytes -= entry.bytes.byte_length();
        Some(entry.bytes)
    }

    pub fn revoke_owner(&mut self, owner: MediaOwner) {
        while let Some(token) = self
            .pending
            .iter()
            .find(|entry| entry.owner == Some(owner))
            .map(|entry| entry.token.clone())
        {
            self.drop_pending(&token);
        }
    }

    /// `pendingHandoffCount()`
    pub fn pending_handoff_count(&self) -> usize {
        self.pending.len()
    }

    /// The timers that are due at `now_ms` fire, the earliest first (a tie in
    /// the order they were set): each drops the handoff of its token.
    pub fn expire(&mut self, now_ms: f64) {
        loop {
            let due = self
                .timers
                .iter()
                .enumerate()
                .filter(|(_, timer)| timer.due_ms <= now_ms)
                .min_by(|(_, a), (_, b)| a.due_ms.total_cmp(&b.due_ms).then(a.id.cmp(&b.id)))
                .map(|(at, _)| at);
            let Some(at) = due else {
                return;
            };
            let timer = self.timers.remove(at);
            self.drop_pending(&timer.token);
        }
    }
}

#[cfg(test)]
mod scoped_tests {
    use super::{ByteHandoffs, MediaOwner};

    #[test]
    fn handoff_is_scoped_to_the_exact_view_lifetime() {
        let a = MediaOwner {
            view: 1,
            generation: 11,
        };
        let other_view = MediaOwner {
            view: 2,
            generation: 11,
        };
        let reused_view = MediaOwner {
            view: 1,
            generation: 12,
        };
        let mut handoffs = ByteHandoffs::new();
        let url = handoffs
            .hand_off_bytes_for(b"secret".to_vec(), "token".into(), 0.0, a)
            .unwrap();
        assert!(handoffs.take_handoff(&url).is_none());
        assert!(handoffs.take_handoff_for(&url, other_view).is_none());
        assert!(handoffs.take_handoff_for(&url, reused_view).is_none());
        assert_eq!(handoffs.take_handoff_for(&url, a), Some(b"secret".to_vec()));
        assert!(handoffs.take_handoff_for(&url, a).is_none());
    }

    #[test]
    fn revocation_preserves_other_owners_handoffs() {
        let a = MediaOwner {
            view: 1,
            generation: 1,
        };
        let b = MediaOwner {
            view: 2,
            generation: 1,
        };
        let mut handoffs = ByteHandoffs::new();
        let url_a = handoffs
            .hand_off_bytes_for(vec![1], "a".into(), 0.0, a)
            .unwrap();
        let url_b = handoffs
            .hand_off_bytes_for(vec![2], "b".into(), 0.0, b)
            .unwrap();
        handoffs.revoke_owner(a);
        assert!(handoffs.take_handoff_for(&url_a, a).is_none());
        assert_eq!(handoffs.take_handoff_for(&url_b, b), Some(vec![2]));
    }

    #[test]
    fn staged_generation_preserves_current_until_commit_and_cannot_revive_after_close() {
        let lease = super::MediaViewLease::new(7).unwrap();
        let original = lease.current().unwrap();
        let failed = lease.stage_document().unwrap();
        assert!(lease.current().unwrap().same_session(&original));
        lease.abort_document(&failed);
        assert!(!failed.is_active());
        assert!(original.is_active());
        let stale = lease.stage_document().unwrap();
        let next = lease.stage_document().unwrap();
        assert!(!stale.is_active());
        assert!(lease.commit_document(&stale).is_err());
        lease.commit_document(&next).unwrap();
        assert!(!original.is_active());
        assert!(lease.current().unwrap().same_session(&next));
        let pending = lease.stage_document().unwrap();
        lease.revoke();
        assert!(!pending.is_active());
        assert!(lease.commit_document(&pending).is_err());
        assert!(lease.current().is_none());
    }

    #[test]
    fn admitted_stage_replaces_only_captured_prior_and_defers_its_notification() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let lease = super::MediaViewLease::new(19).unwrap();
        let old = lease.current().unwrap();
        let prior = lease.stage_document().unwrap();
        let candidate = lease.prepare_document().unwrap();
        let other = super::MediaViewLease::new(19).unwrap();
        let foreign = other.prepare_document().unwrap();
        assert!(
            lease
                .with_prepared_stage(&old, Some(&prior), &foreign, |_| ())
                .is_err()
        );
        assert!(lease.pending().unwrap().same_session(&prior));
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let observed = lease.clone();
        let expected = candidate.permit().clone();
        let _subscription = prior.on_revoke(move || {
            assert!(observed.pending().unwrap().same_session(&expected));
            seen.fetch_add(1, Ordering::SeqCst);
        });
        let mut notifications = super::DeferredMediaLoss::default();
        let mut plan = super::MediaPermitGatePlan::new(&[&old, &prior, candidate.permit()]);
        lease
            .with_prepared_stage(&old, Some(&prior), &candidate, |stage| {
                plan.with_active_retiring(
                    &prior,
                    candidate.permit(),
                    &mut notifications,
                    |admitted| stage.commit(admitted),
                )
                .unwrap()
                .unwrap();
                assert_eq!(count.load(Ordering::SeqCst), 0);
            })
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(notifications);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(old.is_active());
        assert!(!prior.is_active());
        assert!(lease.pending().unwrap().same_session(candidate.permit()));
        assert!(
            lease
                .with_prepared_stage(&old, Some(&prior), &candidate, |_| ())
                .is_err()
        );
    }

    #[test]
    fn admitted_successor_retires_exact_old_once_and_notifies_after_unlock() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let lease = super::MediaViewLease::new(8).unwrap();
        let old = lease.current().unwrap();
        let successor = lease.stage_document().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let observed = lease.clone();
        let expected = successor.clone();
        let _subscription = old.on_revoke(move || {
            assert!(observed.current().unwrap().same_session(&expected));
            seen.fetch_add(1, Ordering::SeqCst);
        });
        let mut notifications = super::DeferredMediaLoss::default();
        let mut plan = super::MediaPermitGatePlan::new(&[&old, &successor]);
        lease
            .with_staged_transition(&old, &successor, |staged| {
                plan.with_active_retiring(&old, &successor, &mut notifications, |admitted| {
                    staged.commit(admitted)
                })
                .unwrap()
                .unwrap();
                assert_eq!(count.load(Ordering::SeqCst), 0);
            })
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(notifications);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(!old.is_active());
        assert!(lease.current().unwrap().same_session(&successor));
        assert!(
            lease
                .with_staged_transition(&old, &successor, |_| ())
                .is_err()
        );
    }

    #[test]
    fn stale_or_unlocked_successor_cannot_commit() {
        let lease = super::MediaViewLease::new(18).unwrap();
        let old = lease.current().unwrap();
        let stale = lease.stage_document().unwrap();
        let successor = lease.stage_document().unwrap();
        let mut notifications = super::DeferredMediaLoss::default();
        assert!(lease.with_staged_transition(&old, &stale, |_| ()).is_err());
        let mut plan = super::MediaPermitGatePlan::new(&[&old]);
        lease
            .with_staged_transition(&old, &successor, |staged| {
                assert!(
                    plan.with_active_retiring(&old, &successor, &mut notifications, |admitted| {
                        staged.commit(admitted)
                    })
                    .is_none()
                );
            })
            .unwrap();
        assert!(old.is_active());
        assert!(lease.current().unwrap().same_session(&old));
    }

    #[test]
    fn exact_permit_gate_deduplicates_aliases_and_rejects_replacement() {
        let lease = super::MediaViewLease::new(9).unwrap();
        let original = lease.current().unwrap();
        let alias = original.clone();
        assert_eq!(
            super::MediaPermit::with_active_all([Some(&original), None, Some(&alias)], || 3),
            Some(3)
        );
        let next = lease.stage_document().unwrap();
        lease.commit_document(&next).unwrap();
        assert_eq!(
            original.with_active(|| panic!("old document effect admitted")),
            None::<()>
        );
        assert_eq!(
            super::MediaPermit::with_active_all([Some(&next), Some(&original)], || 7),
            None
        );
        assert_eq!(next.with_active(|| 11), Some(11));
    }

    #[test]
    fn loss_subscriptions_are_once_late_and_removable() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let permit = super::MediaPermit::mint(10).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let removed_count = count.clone();
        let removed = permit.on_revoke(move || {
            removed_count.fetch_add(100, Ordering::SeqCst);
        });
        drop(removed);
        let first_count = count.clone();
        let _first = permit.on_revoke(move || {
            first_count.fetch_add(1, Ordering::SeqCst);
        });
        permit.revoke();
        permit.revoke();
        let late_count = count.clone();
        let _late = permit.on_revoke(move || {
            late_count.fetch_add(10, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn commit_callbacks_can_reenter_view_and_both_document_permits() {
        let lease = super::MediaViewLease::new(11).unwrap();
        let original = lease.current().unwrap();
        let next = lease.stage_document().unwrap();
        let reenter_lease = lease.clone();
        let reenter_old = original.clone();
        let reenter_next = next.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let _subscription = original.on_revoke(move || {
            assert!(reenter_lease.current().unwrap().same_session(&reenter_next));
            reenter_old.revoke();
            assert_eq!(reenter_next.with_active(|| 1), Some(1));
            let _late = reenter_old.on_revoke(move || {
                send.send(()).unwrap();
            });
        });
        lease.commit_document(&next).unwrap();
        receive.recv().unwrap();
    }

    #[test]
    fn final_effect_and_document_replacement_share_the_actual_permit_gate() {
        let lease = super::MediaViewLease::new(12).unwrap();
        let original = lease.current().unwrap();
        let next = lease.stage_document().unwrap();
        let (entered, received) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let old = original.clone();
        let effect = std::thread::spawn(move || {
            old.with_active(|| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                5
            })
        });
        received.recv().unwrap();
        assert!(original.active.gate.try_lock().is_err());
        let changing = lease.clone();
        let commit = std::thread::spawn(move || changing.commit_document(&next));
        release.send(()).unwrap();
        assert_eq!(effect.join().unwrap(), Some(5));
        commit.join().unwrap().unwrap();
        assert!(!original.is_active());
        assert_eq!(original.with_active(|| 9), None);
    }

    #[test]
    fn opposing_pair_orders_and_aliases_never_take_a_gate_twice() {
        let a = super::MediaPermit::mint(13).unwrap();
        let b = super::MediaPermit::mint(14).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [false, true]
            .into_iter()
            .map(|reverse| {
                let a = a.clone();
                let b = b.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..32 {
                        let permits = if reverse {
                            [Some(&b), Some(&a), Some(&b)]
                        } else {
                            [Some(&a), Some(&b), Some(&a)]
                        };
                        assert_eq!(super::MediaPermit::with_active_all(permits, || 1), Some(1));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn supersede_abort_and_close_notify_only_the_lost_generations() {
        let lease = super::MediaViewLease::new(15).unwrap();
        let current = lease.current().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let current_event = send.clone();
        let _current = current.on_revoke(move || current_event.send("current").unwrap());
        let first = lease.stage_document().unwrap();
        let reenter = lease.clone();
        let first_event = send.clone();
        let first_capture = first.clone();
        let _first = first.on_revoke(move || {
            assert!(!reenter.pending().unwrap().same_session(&first_capture));
            first_event.send("superseded").unwrap();
        });
        let second = lease.stage_document().unwrap();
        assert_eq!(receive.recv().unwrap(), "superseded");
        assert!(current.is_active());
        let reenter = lease.clone();
        let _second = second.on_revoke(move || {
            assert!(reenter.pending().is_none());
            send.send("aborted").unwrap();
        });
        lease.abort_document(&second);
        assert_eq!(receive.recv().unwrap(), "aborted");
        assert!(current.is_active());
        lease.revoke();
        assert_eq!(receive.recv().unwrap(), "current");
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn prepared_permit_plan_reuses_capacity_and_clears_on_unwind() {
        let first = super::MediaPermit::mint(16).unwrap();
        let second = super::MediaPermit::mint(17).unwrap();
        let alias = first.clone();
        let mut plan = super::MediaPermitGatePlan::new(&[&second, &first, &alias]);
        assert_eq!(plan.permits.len(), 2);
        let capacity = plan.guards.capacity();
        assert_eq!(plan.with_active(|| 3), Some(3));
        assert!(plan.guards.is_empty());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            plan.with_active(|| panic!("synthetic final effect panic"));
        }));
        assert!(panic.is_err());
        assert!(plan.guards.is_empty());
        assert_eq!(plan.with_active(|| 5), Some(5));
        assert_eq!(plan.guards.capacity(), capacity);
        second.revoke();
        assert_eq!(plan.with_active(|| 7), None);
        assert!(plan.guards.is_empty());
        assert_eq!(plan.guards.capacity(), capacity);
    }
}
