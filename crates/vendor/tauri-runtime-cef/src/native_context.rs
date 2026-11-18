// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, Weak};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::FrameNavigationState;

/// Header installed only after the browser authenticates a native context.
pub const NATIVE_ATTESTATION_HEADER: &str = "x-seuoffice-native-attestation";

static NATIVE_REQUEST_REGISTRIES: LazyLock<Mutex<HashMap<String, Weak<Shared>>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

/// A header is only an index into native-issued, single-use ingress evidence.
/// JavaScript cannot issue this evidence or choose the registry/context it names.
pub fn redeem_native_context(headers: &http::HeaderMap) -> Option<NativeContextToken> {
  let handle = headers.get(NATIVE_ATTESTATION_HEADER)?.to_str().ok()?;
  let registry = NATIVE_REQUEST_REGISTRIES.lock().ok()?.remove(handle)?.upgrade()?;
  let token = ContextRegistry(registry).redeem(handle)?;
  token.is_verified().then_some(token)
}

pub(crate) struct NativeRequestAttestation {
  registry: Weak<Shared>,
  handle: String,
}

impl NativeRequestAttestation {
  pub(crate) fn header(&self) -> &str { &self.handle }
}

impl Drop for NativeRequestAttestation {
  fn drop(&mut self) {
    NATIVE_REQUEST_REGISTRIES.lock().unwrap_or_else(|error| error.into_inner()).remove(&self.handle);
    if let Some(registry) = self.registry.upgrade() {
      if let Ok(mut state) = registry.state.lock() { state.attestations.remove(&self.handle); }
    }
  }
}

type RevokeCallback = Box<dyn FnOnce() + Send + 'static>;

struct RevokeSlot {
  active: AtomicBool,
  callback: Mutex<Option<RevokeCallback>>,
}

impl RevokeSlot {
  fn take(&self) -> Option<RevokeCallback> {
    if !self.active.swap(false, Ordering::AcqRel) { return None; }
    self.callback.lock().unwrap_or_else(|poison| poison.into_inner()).take()
  }
}

/// Retain this subscription while owner loss must be observed. Dropping it
/// unregisters the callback unless revocation has already claimed it.
pub struct NativeContextRevocation {
  slot: Arc<RevokeSlot>,
}

impl Drop for NativeContextRevocation {
  fn drop(&mut self) {
    let callback = self.slot.take();
    drop(callback);
  }
}

/// Callback delivery separated from the short authority transition. Native
/// pre-mutation hooks must retain this value and finish it only after leaving
/// Chromium's non-reentrant critical section.
#[must_use]
pub(crate) struct DeferredContextRevocation {
  origin: NativeContextToken,
  listeners: Vec<Arc<RevokeSlot>>,
  delivery: Option<Box<dyn FnOnce(NativeContextToken) + Send>>,
}

impl DeferredContextRevocation {
  pub(crate) fn with_delivery(mut self, delivery: impl FnOnce(NativeContextToken) + Send + 'static) -> Self {
    self.delivery = Some(Box::new(delivery));
    self
  }

  pub(crate) fn finish(self) -> NativeContextToken {
    let loss = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify_revocations(self.listeners)));
    if let Some(delivery) = self.delivery { delivery(self.origin.clone()); }
    if let Err(panic) = loss { std::panic::resume_unwind(panic); }
    self.origin
  }
}

static DEFERRED_REVOCATIONS: LazyLock<Mutex<Vec<DeferredContextRevocation>>> =
  LazyLock::new(|| Mutex::new(Vec::new()));

/// Pre-mutation delivery is process-owned, not tied to a browser or CEF task.
/// Posting failure and browser destruction cannot drop a retained revocation.
pub(crate) fn retain_deferred_revocation(revocation: DeferredContextRevocation) {
  DEFERRED_REVOCATIONS.lock().unwrap_or_else(|error| error.into_inner()).push(revocation);
}

/// Run outside native mutation/destruction callbacks. Draining a batch never
/// holds the queue lock while application cancellation callbacks execute.
pub(crate) fn drain_deferred_revocations() {
  loop {
    let batch = {
      let mut queue = DEFERRED_REVOCATIONS.lock().unwrap_or_else(|error| error.into_inner());
      if queue.is_empty() { return; }
      std::mem::take(&mut *queue)
    };
    for revocation in batch {
      if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| revocation.finish())).is_err() {
        log::error!("native context cancellation callback panicked");
      }
    }
  }
}

fn notify_revocations(slots: Vec<Arc<RevokeSlot>>) {
  let mut first_panic = None;
  for slot in slots {
    if let Some(callback) = slot.take()
      && let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback))
      && first_panic.is_none() {
      first_panic = Some(panic);
    }
  }
  if let Some(panic) = first_panic {
    std::panic::resume_unwind(panic);
  }
}

/// Exact identity supplied by the private RFH-bound CEF registration callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedNativeIdentity {
  pub(crate) renderer_child_id: i32,
  pub(crate) document_token: String,
  pub(crate) context_token: String,
  pub(crate) world_id: i32,
  pub(crate) registration: u64,
}

struct Record {
  browser_id: i32,
  frame_id: String,
  capability: String,
  browser: FrameNavigationState,
  challenge: Option<(String, String)>,
  unique_context_id: Option<String>,
  verified_identity: Option<VerifiedNativeIdentity>,
  document_ready: bool,
  navigation_issuance: Option<u64>,
  process_identity: Arc<OnceLock<crate::NativeRendererProcessIdentity>>,
  endpoints: Option<crate::native_invoke::NativeContextEndpoints>,
  process_exit: Option<crate::NativeRendererProcessSubscription>,
  revoked: Arc<AtomicBool>,
  listeners: Vec<Arc<RevokeSlot>>,
}

impl Record {
  fn revoke(self) -> Vec<Arc<RevokeSlot>> {
    self.revoked.store(true, Ordering::Release);
    self.listeners
  }
}

struct PendingIntent {
  browser: FrameNavigationState,
  browser_id: i32,
  issuance: u64,
}

struct InvocationTarget {
  browser: FrameNavigationState,
  browser_id: i32,
  target: Arc<crate::native_invoke::NativeInvocationTarget>,
}

#[derive(Default)]
struct State {
  contexts: HashMap<String, Record>,
  attestations: HashMap<String, String>,
  intents: HashMap<String, PendingIntent>,
  targets: Vec<InvocationTarget>,
}

#[derive(Default)]
struct Shared {
  state: Mutex<State>,
  changed: Condvar,
  pending: AtomicUsize,
}

impl Drop for Shared {
  fn drop(&mut self) {
    let slots = {
      let state = self.state.get_mut().unwrap_or_else(|poison| poison.into_inner());
      state.contexts.drain().flat_map(|(_, record)| record.revoke()).collect()
    };
    notify_revocations(slots);
  }
}

impl std::fmt::Debug for ContextRegistry {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("ContextRegistry([redacted])")
  }
}

#[derive(Clone, Default)]
pub(crate) struct ContextRegistry(Arc<Shared>);

pub(crate) struct PendingAdmission(Arc<Shared>);

impl Drop for PendingAdmission {
  fn drop(&mut self) { self.0.pending.fetch_sub(1, Ordering::AcqRel); }
}

impl ContextRegistry {
  pub(crate) fn reserve_admission(&self) -> Option<PendingAdmission> {
    self.0.pending.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
      (count < 256).then_some(count + 1)
    }).ok()?;
    Some(PendingAdmission(self.0.clone()))
  }
}

/// An authenticated native V8 context, not a webview label or navigation snapshot.
#[derive(Clone)]
pub struct NativeContextToken {
  registry: Weak<Shared>,
  capability: String,
  browser: FrameNavigationState,
  browser_id: i32,
  frame_id: String,
  revoked: Arc<AtomicBool>,
  pub(crate) navigation_issuance: Option<u64>,
  pub(crate) native_identity: Option<VerifiedNativeIdentity>,
  pub(crate) process_identity: Arc<OnceLock<crate::NativeRendererProcessIdentity>>,
}

/// Evidence that the private native lifecycle reported readiness for this exact
/// registered context. It cannot be constructed from a frame or a URL.
#[derive(Clone)]
pub struct NativeDocumentReady { origin: NativeContextToken }

impl NativeDocumentReady {
  pub fn origin(&self) -> &NativeContextToken { &self.origin }
  pub fn is_live(&self) -> bool { self.origin.is_verified() }
  pub fn on_revoke(&self, callback: impl FnOnce() + Send + 'static) -> Option<NativeContextRevocation> {
    self.origin.on_revoke(callback)
  }
  /// The operation must be short and must not call CEF, UI, or user code.
  pub fn with_live<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
    self.origin.with_live(operation)
  }
}

/// Prepared outside every authority gate, on the final effect's thread.
pub struct NativeContextGatePlan<'a> {
  tokens: Vec<&'a NativeContextToken>,
  registries: Vec<Arc<Shared>>,
  indices: Vec<usize>,
  guards: Vec<Option<std::sync::MutexGuard<'static, State>>>,
  retired: Vec<HashMap<String, Record>>,
}

impl<'a> NativeContextGatePlan<'a> {
  pub fn new(tokens: &[&'a NativeContextToken]) -> Option<Self> {
    if tokens.is_empty() { return None; }
    let mut registries: Vec<_> = tokens.iter().map(|token| token.registry.upgrade()).collect::<Option<_>>()?;
    registries.sort_unstable_by_key(Arc::as_ptr);
    registries.dedup_by(|left, right| Arc::ptr_eq(left, right));
    let indices = tokens.iter().map(|token| registries.iter().position(|registry|
      Weak::ptr_eq(&Arc::downgrade(registry), &token.registry))).collect::<Option<Vec<_>>>()?;
    let guards = std::iter::repeat_with(|| None).take(registries.len()).collect();
    let retired = std::iter::repeat_with(HashMap::new).take(registries.len()).collect();
    Some(Self { tokens: tokens.to_vec(), registries, indices, guards, retired })
  }

  /// Consuming the plan keeps lock storage local without self-referential borrows.
  /// Acquisition, validation and the effect allocate no guard/index collections.
  pub fn with_live<T>(self, operation: impl FnOnce() -> T) -> Option<T> {
    let registries = self.registries;
    let mut guards: Vec<Option<std::sync::MutexGuard<'_, State>>> = self.guards;
    let mut retired = self.retired;
    for (index, registry) in registries.iter().enumerate() {
      guards[index] = Some(registry.state.lock().ok()?);
    }
    for (token, index) in self.tokens.iter().zip(self.indices) {
      if !token.matches_record(guards[index].as_ref()?) { return None; }
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
      Ok(value) => Some(value),
      Err(panic) => {
        for (index, slot) in guards.iter_mut().enumerate() {
          let state = slot.as_mut().expect("prepared native registry guard");
          for record in state.contexts.values() { record.revoked.store(true, Ordering::Release); }
          retired[index] = std::mem::take(&mut state.contexts);
          state.attestations.clear();
          state.intents.clear();
          registries[index].changed.notify_all();
        }
        drop(guards);
        for records in retired {
          for record in records.into_values() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify_revocations(record.listeners)));
          }
        }
        std::panic::resume_unwind(panic)
      }
    }
  }
}

impl std::fmt::Debug for NativeContextToken {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.debug_struct("NativeContextToken").finish_non_exhaustive()
  }
}

impl PartialEq for NativeContextToken {
  fn eq(&self, other: &Self) -> bool {
    Weak::ptr_eq(&self.registry, &other.registry) && self.capability == other.capability
  }
}
impl Eq for NativeContextToken {}

impl NativeContextToken {
  pub fn browser_id(&self) -> i32 { self.browser_id }
  pub fn frame_id(&self) -> &str { &self.frame_id }
  pub fn browser_state(&self) -> &FrameNavigationState { &self.browser }

  /// Selects a committed snapshot only after the original native RFH and
  /// DocumentToken still match. This never promotes a pending context.
  pub fn capture_current_document(&self, webview: &crate::Webview) -> Option<crate::NativeDocumentToken> {
    use cef::{currently_on, sys, ImplBrowser, ImplNativeContextEvaluation};
    if currently_on(sys::cef_thread_id_t::TID_UI.into()) != 1
      || webview.browser().identifier() != self.browser_id
      || !self.browser.is_same_browser(webview.frame_navigation_state()) {
      return None;
    }
    let endpoints = self.endpoints()?;
    if endpoints.evaluation.is_current_document() != 1 { return None; }
    let document = webview.snapshot().document.clone()?;
    self.is_verified().then_some(document)
  }

  /// Negative-only notification, including deferred native pre-mutation revoke.
  /// A false result is not authority and never replaces `with_live`.
  pub fn is_revoked(&self) -> bool { self.revoked.load(Ordering::Acquire) }

  pub(crate) fn endpoints(&self) -> Option<crate::native_invoke::NativeContextEndpoints> {
    let registry = self.registry.upgrade()?;
    let state = registry.state.lock().ok()?;
    let record = state.contexts.get(&self.capability)?;
    if record.verified_identity.is_none() || record.browser_id != self.browser_id
      || record.frame_id != self.frame_id || !record.browser.is_same_browser(&self.browser)
      || !self.browser.has_browser_id(self.browser_id) { return None; }
    record.endpoints.clone()
  }

  pub fn reply_sink(&self) -> Option<crate::NativeReplySink> {
    let endpoints = self.endpoints()?;
    Some(crate::NativeReplySink { origin: self.clone(), channel: endpoints.channel })
  }

  /// Called only by the native private-channel ingress, never a scheme
  /// request's capability/header parser. The guard removes unused evidence.
  pub(crate) fn attest_native_request(&self) -> Option<NativeRequestAttestation> {
    let registry = self.registry.upgrade()?;
    let handle = crate::cef_impl::trusted_init::random_capability()?;
    let mut state = registry.state.lock().ok()?;
    let record = state.contexts.get(&self.capability)?;
    if record.verified_identity.is_none() || record.browser_id != self.browser_id
      || record.frame_id != self.frame_id || !record.browser.is_same_browser(&self.browser)
      || !self.browser.has_browser_id(self.browser_id) || state.attestations.len() >= 4096 {
      return None;
    }
    let mut requests = NATIVE_REQUEST_REGISTRIES.lock().ok()?;
    if requests.len() >= 4096 || requests.contains_key(&handle) || state.attestations.contains_key(&handle) { return None; }
    state.attestations.insert(handle.clone(), self.capability.clone());
    requests.insert(handle.clone(), Arc::downgrade(&registry));
    Some(NativeRequestAttestation { registry: Arc::downgrade(&registry), handle })
  }
  pub fn is_live(&self) -> bool {
    let Some(registry) = self.registry.upgrade() else { return false };
    let live = registry.state.lock().is_ok_and(|state| state.contexts.get(&self.capability).is_some_and(|record| {
      record.browser_id == self.browser_id
        && record.frame_id == self.frame_id && record.browser.is_same_browser(&self.browser)
        && self.browser.has_browser_id(self.browser_id)
    }));
    live
  }

  /// True only for a live token minted from the private CEF registration path.
  pub fn is_verified(&self) -> bool {
    let Some(registry) = self.registry.upgrade() else { return false };
    registry.state.lock().is_ok_and(|state| state.contexts.get(&self.capability).is_some_and(|record| {
      record.verified_identity.is_some() && record.browser_id == self.browser_id
        && record.frame_id == self.frame_id && record.browser.is_same_browser(&self.browser)
        && self.browser.has_browser_id(self.browser_id)
    }))
  }

  /// Return readiness only after the native exact-context lifecycle callback.
  pub fn document_ready(&self) -> Option<NativeDocumentReady> {
    let registry = self.registry.upgrade()?;
    let state = registry.state.lock().ok()?;
    let record = state.contexts.get(&self.capability)?;
    if !record.document_ready || record.verified_identity.is_none()
      || record.verified_identity != self.native_identity || record.browser_id != self.browser_id
      || record.frame_id != self.frame_id || !record.browser.is_same_browser(&self.browser)
      || !self.browser.has_browser_id(self.browser_id) { return None; }
    Some(NativeDocumentReady { origin: self.clone() })
  }

  pub(crate) fn mark_document_ready(&self) -> bool {
    let Some(registry) = self.registry.upgrade() else { return false };
    let Ok(mut state) = registry.state.lock() else { return false };
    let Some(record) = state.contexts.get_mut(&self.capability) else { return false };
    if record.document_ready || record.verified_identity.is_none()
      || record.verified_identity != self.native_identity || record.browser_id != self.browser_id
      || record.frame_id != self.frame_id || !record.browser.is_same_browser(&self.browser)
      || !self.browser.has_browser_id(self.browser_id) { return false; }
    record.document_ready = true;
    true
  }

  /// Subscribe atomically against registry revocation without waiting for its
  /// lock. `None` means already revoked, unavailable or contended and must be
  /// treated as owner loss. Keep the returned handle alive to stay subscribed.
  /// The callback runs at most once, outside the registry lock; it may already
  /// have been claimed when the handle is dropped.
  pub fn on_revoke(&self, callback: impl FnOnce() + Send + 'static) -> Option<NativeContextRevocation> {
    let registry = self.registry.upgrade()?;
    let mut state = registry.state.try_lock().ok()?;
    let record = state.contexts.get_mut(&self.capability)?;
    if record.browser_id != self.browser_id || record.frame_id != self.frame_id
      || !record.browser.is_same_browser(&self.browser) || !self.browser.try_has_browser_id(self.browser_id) {
      return None;
    }
    record.listeners.retain(|listener| listener.active.load(Ordering::Acquire));
    let slot = Arc::new(RevokeSlot { active: AtomicBool::new(true), callback: Mutex::new(Some(Box::new(callback))) });
    record.listeners.push(slot.clone());
    Some(NativeContextRevocation { slot })
  }

  /// Runs a short admission or commit operation atomically against native
  /// context revocation. The closure must not wait for CEF, UI, or user input.
  pub fn with_live<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
    Self::with_live_all([Some(self)], operation)
  }

  /// Fence both captured owners without recursively locking a shared registry.
  /// The closure must not enter UI, CEF, modal, or core waits.
  pub fn with_live_pair<T>(&self, other: &Self, operation: impl FnOnce() -> T) -> Option<T> {
    Self::with_live_all([Some(self), Some(other)], operation)
  }

  /// Fence all original native owners in canonical registry order. Aliases
  /// share one lock. No application callbacks run while authority is removed.
  pub fn with_live_all<T, const N: usize>(tokens: [Option<&Self>; N], operation: impl FnOnce() -> T) -> Option<T> {
    if tokens.iter().all(Option::is_none) { return None; }
    let mut registries = tokens.map(|token| token.and_then(|token| token.registry.upgrade()));
    if tokens.iter().zip(&registries).any(|(token, registry)| token.is_some() && registry.is_none()) { return None; }
    registries.sort_unstable_by_key(|registry| registry.as_ref().map(Arc::as_ptr));
    let mut unavailable = false;
    let mut guards: [Option<std::sync::MutexGuard<'_, State>>; N] = std::array::from_fn(|index| {
      let registry = registries[index].as_ref()?;
      if index > 0 && registries[index - 1].as_ref().is_some_and(|previous| Arc::ptr_eq(previous, registry)) { return None; }
      match registry.state.lock() {
        Ok(state) => Some(state),
        Err(_) => { unavailable = true; None }
      }
    });
    if unavailable { return None; }
    for token in tokens.into_iter().flatten() {
      let index = registries.iter().position(|registry|
        registry.as_ref().is_some_and(|registry| Weak::ptr_eq(&Arc::downgrade(registry), &token.registry)))?;
      if !token.matches_record(guards[index].as_ref()?) { return None; }
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
      Ok(value) => Some(value),
      Err(panic) => {
        let notifications: [Vec<Arc<RevokeSlot>>; N] = std::array::from_fn(|index| {
          let Some(state) = guards[index].as_mut() else { return Vec::new() };
          let listeners = state.contexts.drain().flat_map(|(_, record)| record.revoke()).collect();
          state.attestations.clear();
          state.intents.clear();
          registries[index].as_ref().expect("locked registry").changed.notify_all();
          listeners
        });
        drop(guards);
        for listeners in notifications {
          let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify_revocations(listeners)));
        }
        std::panic::resume_unwind(panic)
      }
    }
  }

  fn matches_record(&self, state: &State) -> bool {
    state.contexts.get(&self.capability).is_some_and(|record|
      record.browser_id == self.browser_id && record.frame_id == self.frame_id
        && record.browser.is_same_browser(&self.browser)
        && self.browser.has_browser_id(self.browser_id))
  }
}

/// A native V8 context additionally bound to one CDP unique execution context.
#[derive(Clone, Debug)]
pub struct NativeEvalContext {
  token: NativeContextToken,
  unique_context_id: String,
}

impl NativeEvalContext {
  pub fn unique_context_id(&self) -> &str { &self.unique_context_id }
  pub fn is_live(&self) -> bool {
    let Some(registry) = self.token.registry.upgrade() else { return false };
    let live = registry.state.lock().is_ok_and(|state| state.contexts.get(&self.token.capability).is_some_and(|record| {
      record.unique_context_id.as_deref() == Some(self.unique_context_id.as_str())
        && record.browser.is_same_browser(&self.token.browser)
        && self.token.browser.has_browser_id(self.token.browser_id)
    }));
    live
  }
}

impl NativeContextToken {
  /// Immutable process incarnation captured when this document registered.
  /// This identity is not a liveness or authorization check.
  pub fn renderer_process_identity(&self) -> Option<crate::NativeRendererProcessIdentity> {
    self.process_identity.get().cloned()
  }

  pub fn renderer_process(&self) -> Option<crate::NativeRendererProcess> {
    self.endpoints().map(|endpoints| endpoints.renderer_process)
  }
  pub fn eval_context(&self) -> Option<NativeEvalContext> {
    let registry = self.registry.upgrade()?;
    let state = registry.state.lock().ok()?;
    let record = state.contexts.get(&self.capability)?;
    if !record.browser.is_same_browser(&self.browser) || !self.browser.has_browser_id(self.browser_id) { return None; }
    Some(NativeEvalContext { token: self.clone(), unique_context_id: record.unique_context_id.clone()? })
  }
}

impl ContextRegistry {
  pub(crate) fn menu_context(&self, browser: &FrameNavigationState, browser_id: i32) -> Option<NativeContextToken> {
    let state = self.0.state.lock().ok()?;
    if !browser.has_browser_id(browser_id) { return None; }
    let mut matches = state.contexts.values().filter(|record|
      record.browser_id == browser_id && record.browser.is_same_browser(browser)
        && record.verified_identity.as_ref().is_some_and(|identity| identity.world_id == 0)
        && record.endpoints.is_some());
    let record = matches.next()?;
    if matches.next().is_some() { return None; }
    Some(NativeContextToken { registry: Arc::downgrade(&self.0), capability: record.capability.clone(),
      browser: browser.clone(), browser_id, frame_id: record.frame_id.clone(), revoked: record.revoked.clone(),
      navigation_issuance: record.navigation_issuance, native_identity: record.verified_identity.clone(),
      process_identity: record.process_identity.clone() })
  }

  pub(crate) fn verified_popup_token(&self, browser: &FrameNavigationState, browser_id: i32,
    identity: &VerifiedNativeIdentity) -> Option<NativeContextToken> {
    let state = self.0.state.lock().ok()?;
    if !browser.has_browser_id(browser_id) || identity.world_id != 0 { return None; }
    let mut matches = state.contexts.values().filter(|record|
      record.browser_id == browser_id && record.browser.is_same_browser(browser)
        && record.verified_identity.as_ref() == Some(identity) && record.endpoints.is_some());
    let record = matches.next()?;
    if matches.next().is_some() { return None; }
    Some(NativeContextToken { registry: Arc::downgrade(&self.0), capability: record.capability.clone(),
      browser: browser.clone(), browser_id, frame_id: record.frame_id.clone(), revoked: record.revoked.clone(),
      navigation_issuance: record.navigation_issuance, native_identity: record.verified_identity.clone(),
      process_identity: record.process_identity.clone() })
  }

  pub(crate) fn verified_resource_token(&self, browser: &FrameNavigationState,
    origin: &cef::NativeResourceOrigin) -> Option<NativeContextToken> {
    if origin.registration == 0 || origin.world_id != 0 { return None; }
    let identity = VerifiedNativeIdentity {
      renderer_child_id: origin.renderer_child_id,
      document_token: format!("{:016X}{:016X}", origin.document_high, origin.document_low),
      context_token: format!("{:016X}{:016X}", origin.context_high, origin.context_low),
      world_id: origin.world_id,
      registration: origin.registration,
    };
    let token = self.verified_popup_token(browser, origin.browser_id, &identity)?;
    (token.navigation_issuance == Some(origin.registration)).then_some(token)
  }

  pub(crate) fn bind_invocation_target(&self, browser: FrameNavigationState, browser_id: i32,
    target: Arc<crate::native_invoke::NativeInvocationTarget>) -> bool {
    let Ok(mut state) = self.0.state.lock() else { return false };
    if !browser.has_browser_id(browser_id) || state.targets.len() >= 256
      || state.targets.iter().any(|bound| bound.browser.is_same_browser(&browser)) {
      return false;
    }
    state.targets.push(InvocationTarget { browser, browser_id, target });
    true
  }

  pub(crate) fn invocation_target(&self, browser: &FrameNavigationState, browser_id: i32)
    -> Option<Arc<crate::native_invoke::NativeInvocationTarget>> {
    let state = self.0.state.lock().ok()?;
    if !browser.has_browser_id(browser_id) { return None; }
    state.targets.iter().find(|bound| bound.browser_id == browser_id
      && bound.browser.is_same_browser(browser)).map(|bound| bound.target.clone())
  }

  pub(crate) fn remove_invocation_target(&self, browser: &FrameNavigationState) {
    let Ok(mut state) = self.0.state.lock() else { return };
    let retired = state.targets.iter().position(|bound| bound.browser.is_same_browser(browser))
      .map(|index| state.targets.swap_remove(index));
    drop(state);
    drop(retired);
  }

  pub(crate) fn attach_endpoints(&self, token: &NativeContextToken,
    endpoints: crate::native_invoke::NativeContextEndpoints) -> bool {
    if !Weak::ptr_eq(&token.registry, &Arc::downgrade(&self.0)) { return false; }
    let Ok(mut state) = self.0.state.lock() else { return false };
    let Some(record) = state.contexts.get_mut(&token.capability) else { return false };
    if record.endpoints.is_some() || record.verified_identity.is_none()
      || record.browser_id != token.browser_id || record.frame_id != token.frame_id
      || !record.browser.is_same_browser(&token.browser)
      || !token.browser.has_browser_id(token.browser_id) { return false; }
    let process_identity = endpoints.renderer_process.identity();
    if record.verified_identity.as_ref().is_none_or(|identity| identity.renderer_child_id != process_identity.child_id())
      || record.process_identity.set(process_identity).is_err() { return false; }
    record.endpoints = Some(endpoints);
    true
  }

  /// Bind an exit observer to this exact registered document and process.
  /// The callback holds only a weak registry, so the record cannot retain itself.
  pub(crate) fn observe_process_exit(
    &self, token: &NativeContextToken,
    handler: Option<Arc<crate::FrameEventHandler>>,
  ) -> bool {
    let Some(process) = token.renderer_process() else { return false };
    let identity = process.identity();
    let registry = Arc::downgrade(&self.0);
    let capability = token.capability.clone();
    let exit_identity = identity.clone();
    let handler = handler.as_ref().map(Arc::downgrade);
    let subscription = match process.when_lost(move |result| {
      let Some(shared) = registry.upgrade() else { return };
      let registry = ContextRegistry(shared);
      let Some(revocation) = registry.revoke_process_document(&capability, &exit_identity) else { return };
      let revocation = revocation.with_delivery(move |origin| {
        origin.browser_state().native_reload.document_lost(&origin);
        if result.is_ok() {
          if let Some(handler) = handler.and_then(|handler| handler.upgrade()) {
            handler(crate::FrameEvent {
              browser_id: origin.browser_id(), frame_id: origin.frame_id().to_owned(), is_main: true,
              kind: crate::FrameEventKind::NativeRendererTerminated { origin, process: exit_identity },
            });
          }
        }
      });
      retain_deferred_revocation(revocation);
      crate::cef_impl::ipc::on_native_context_did_revoke();
    }) {
      Ok(subscription) => subscription,
      Err(_) => return false,
    };
    let Ok(mut state) = self.0.state.lock() else { return false };
    let Some(record) = state.contexts.get_mut(&token.capability) else { return false };
    if record.process_exit.is_some() || record.process_identity.get() != Some(&identity)
      || record.verified_identity != token.native_identity
      || !record.browser.is_same_browser(&token.browser) { return false; }
    record.process_exit = Some(subscription);
    true
  }

  fn revoke_process_document(&self, capability: &str, process: &crate::NativeRendererProcessIdentity)
    -> Option<DeferredContextRevocation> {
    let mut state = self.0.state.lock().ok()?;
    if !state.contexts.get(capability).is_some_and(|record|
      record.process_identity.get() == Some(process) && record.verified_identity.is_some()) { return None; }
    let record = state.contexts.remove(capability)?;
    record.revoked.store(true, Ordering::Release);
    state.attestations.retain(|_, context| context != capability);
    self.0.changed.notify_all();
    let origin = NativeContextToken {
      registry: Arc::downgrade(&self.0), capability: capability.to_owned(),
      browser: record.browser.clone(), browser_id: record.browser_id,
      frame_id: record.frame_id.clone(), revoked: record.revoked.clone(),
      navigation_issuance: record.navigation_issuance,
      native_identity: record.verified_identity.clone(), process_identity: record.process_identity.clone(),
    };
    let listeners = record.listeners;
    drop(state);
    Some(DeferredContextRevocation { origin, listeners, delivery: None })
  }

  pub(crate) fn register(&self, browser: FrameNavigationState, browser_id: i32, frame_id: String, capability: String) -> bool {
    self.register_inner(browser, browser_id, frame_id, capability, None, None)
  }

  /// Issue before calling the browser-only approved-navigation entry point.
  /// A newer issuance invalidates an earlier unredeemed intent for this browser.
  pub(crate) fn issue_navigation_intent(&self, browser: FrameNavigationState, browser_id: i32) -> Option<(u64, String)> {
    if !browser.has_browser_id(browser_id)
      || !matches!(browser.trusted_init_mode(), Some(crate::TrustedInitMode::ProtectedInvoke | crate::TrustedInitMode::ProtectedPreload)) {
      return None;
    }
    let intent = crate::cef_impl::trusted_init::random_capability()?;
    let mut state = self.0.state.lock().ok()?;
    if !browser.has_browser_id(browser_id) || state.intents.contains_key(&intent) { return None; }
    state.intents.retain(|_, pending| !pending.browser.is_same_browser(&browser));
    if state.intents.len() >= 256 { return None; }
    let issuance = browser.next_navigation_issuance(browser_id)?;
    state.intents.insert(intent.clone(), PendingIntent { browser, browser_id, issuance });
    Some((issuance, intent))
  }

  /// A late failed-load completion must not cancel a successor's issuance.
  pub(crate) fn cancel_navigation_intent(&self, browser: &FrameNavigationState, intent: &str) -> bool {
    let Ok(mut state) = self.0.state.lock() else { return false };
    if state.intents.get(intent).is_some_and(|pending| pending.browser.is_same_browser(browser)) {
      state.intents.remove(intent);
      return true;
    }
    false
  }

  pub(crate) fn has_navigation_intent(&self, browser: &FrameNavigationState, issuance: u64, intent: &str) -> bool {
    self.0.state.lock().is_ok_and(|state| state.intents.get(intent).is_some_and(|pending|
      pending.browser.is_same_browser(browser) && pending.issuance == issuance
        && browser.has_navigation_issuance(pending.browser_id, issuance)))
  }

  pub(crate) fn cancel_browser_intents(&self, browser: &FrameNavigationState) {
    if let Ok(mut state) = self.0.state.lock() {
      state.intents.retain(|_, pending| !pending.browser.is_same_browser(browser));
    }
  }

  /// Called only from the private CEF callback after its RFH/document lineage
  /// check. The authority is minted in the browser and never returned to JS.
  pub(crate) fn register_verified(
    &self,
    browser: FrameNavigationState,
    browser_id: i32,
    frame_id: String,
    identity: VerifiedNativeIdentity,
    issuance: u64,
    intent: &str,
  ) -> Option<NativeContextToken> {
    fn native_token(value: &str) -> bool {
      value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
    }
    if !matches!(browser.trusted_init_mode(), Some(crate::TrustedInitMode::ProtectedInvoke | crate::TrustedInitMode::ProtectedPreload))
      || identity.renderer_child_id <= 0 || identity.world_id != 0
      || identity.registration == 0 || identity.registration != issuance
      || !native_token(&identity.document_token) || !native_token(&identity.context_token) {
      return None;
    }
    let capability = crate::cef_impl::trusted_init::random_capability()?;
    if !self.register_inner(browser.clone(), browser_id, frame_id.clone(), capability.clone(), Some(identity.clone()), Some((intent, issuance))) {
      return None;
    }
    let state = self.0.state.lock().ok()?;
    let record = state.contexts.get(&capability)?;
    let revoked = record.revoked.clone();
    let process_identity = record.process_identity.clone();
    let token = NativeContextToken { registry: Arc::downgrade(&self.0), capability, browser, browser_id, frame_id,
      revoked, navigation_issuance: Some(issuance), native_identity: Some(identity), process_identity };
    drop(state);
    token.is_verified().then_some(token)
  }

  /// Resolve only the identity captured by the private native channel, never
  /// the context currently associated with a frame or a page-supplied header.
  pub(crate) fn verified_token(
    &self,
    browser: &FrameNavigationState,
    browser_id: i32,
    frame_id: &str,
    identity: &VerifiedNativeIdentity,
  ) -> Option<NativeContextToken> {
    let state = self.0.state.lock().ok()?;
    if !browser.has_browser_id(browser_id) { return None; }
    let record = state.contexts.values().find(|record|
      record.browser_id == browser_id && record.frame_id == frame_id
        && record.browser.is_same_browser(browser)
        && record.verified_identity.as_ref() == Some(identity))?;
    Some(NativeContextToken {
      registry: Arc::downgrade(&self.0), capability: record.capability.clone(),
      browser: browser.clone(), browser_id, frame_id: frame_id.to_owned(),
      revoked: record.revoked.clone(),
      navigation_issuance: record.navigation_issuance,
      native_identity: record.verified_identity.clone(),
      process_identity: record.process_identity.clone(),
    })
  }

  fn register_inner(
    &self,
    browser: FrameNavigationState,
    browser_id: i32,
    frame_id: String,
    capability: String,
    verified_identity: Option<VerifiedNativeIdentity>,
    intent: Option<(&str, u64)>,
  ) -> bool {
    if !browser.has_browser_id(browser_id) || frame_id.is_empty() || frame_id.len() > 512
      || capability.len() != 64 || !capability.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
      return false;
    }
    let Ok(mut state) = self.0.state.lock() else { return false };
    if !browser.has_browser_id(browser_id) || state.contexts.len() >= 256 || state.contexts.contains_key(&capability) { return false; }
    if verified_identity.is_some() {
      let Some((intent, issuance)) = intent else { return false };
      if !state.intents.get(intent).is_some_and(|pending|
        pending.browser_id == browser_id && pending.browser.is_same_browser(&browser)
          && pending.issuance == issuance && browser.has_navigation_issuance(browser_id, issuance)) {
        return false;
      }
    } else if intent.is_some() {
      return false;
    }
    // A later callback is not proof that its document superseded this one.
    // Only an exact native revocation may remove a verified registration.
    if state.contexts.values().any(|old| old.verified_identity.is_some()
      && old.browser_id == browser_id && old.frame_id == frame_id
      && old.browser.is_same_browser(&browser)) {
      return false;
    }
    if let Some((intent, _)) = intent { state.intents.remove(intent); }
    let mut revoked = Vec::new();
    state.contexts.retain(|_, old| {
      let keep = old.browser_id != browser_id || old.frame_id != frame_id || !old.browser.is_same_browser(&browser);
      if !keep {
        old.revoked.store(true, Ordering::Release);
        revoked.append(&mut old.listeners);
      }
      keep
    });
    let State { contexts, attestations, .. } = &mut *state;
    attestations.retain(|_, context| contexts.contains_key(context));
    state.contexts.insert(capability.clone(), Record { browser_id, frame_id, capability, browser, challenge: None, unique_context_id: None, verified_identity, document_ready: false, navigation_issuance: intent.map(|(_, issuance)| issuance), process_identity: Arc::new(OnceLock::new()), endpoints: None, process_exit: None, revoked: Arc::new(AtomicBool::new(false)), listeners: Vec::new() });
    self.0.changed.notify_all();
    drop(state);
    notify_revocations(revoked);
    true
  }

  pub(crate) fn challenge(&self, capability: &str, unique_context_id: &str) -> Option<String> {
    if unique_context_id.is_empty() || unique_context_id.len() > 512 { return None; }
    let nonce = crate::cef_impl::trusted_init::random_capability()?;
    let mut state = self.0.state.lock().ok()?;
    let record = state.contexts.get_mut(capability)?;
    if record.unique_context_id.is_some() || record.challenge.is_some() { return None; }
    record.challenge = Some((nonce.clone(), unique_context_id.to_owned()));
    Some(nonce)
  }

  /// `unique_context_id` must be the default execution context of a CDP frame
  /// in the current frame tree. The caller must establish that before calling.
  pub(crate) fn confirm(&self, capability: &str, nonce: &str, unique_context_id: &str, proof: &str) -> bool {
    let Ok(mut state) = self.0.state.lock() else { return false };
    let Some(record) = state.contexts.get_mut(capability) else { return false };
    let Some((expected_nonce, expected_context)) = record.challenge.take() else { return false };
    if expected_nonce != nonce || expected_context != unique_context_id
      || !constant_time_eq(&challenge_response(&record.capability, nonce, unique_context_id), proof) {
      return false;
    }
    record.unique_context_id = Some(unique_context_id.to_owned());
    // CDP binding never decides which native context is current.
    self.0.changed.notify_all();
    true
  }

  pub(crate) fn revoke(&self, browser_id: i32, frame_id: &str, capability: &str) {
    let Ok(mut state) = self.0.state.lock() else { return };
    let revoked = if state.contexts.get(capability).is_some_and(|record| record.browser_id == browser_id && record.frame_id == frame_id) {
      let record = state.contexts.remove(capability).expect("checked context");
      state.attestations.retain(|_, context| context != capability);
      self.0.changed.notify_all();
      record.revoke()
    } else {
      Vec::new()
    };
    drop(state);
    notify_revocations(revoked);
  }

  /// A late CEF release addresses the captured process/document/V8 identity,
  /// never whichever context currently occupies the frame.
  pub(crate) fn revoke_verified(
    &self,
    browser: &FrameNavigationState,
    browser_id: i32,
    frame_id: &str,
    identity: &VerifiedNativeIdentity,
  ) -> Option<NativeContextToken> {
    self.revoke_verified_deferred(browser, browser_id, frame_id, identity)
      .map(DeferredContextRevocation::finish)
  }

  pub(crate) fn revoke_verified_deferred(
    &self,
    browser: &FrameNavigationState,
    browser_id: i32,
    frame_id: &str,
    identity: &VerifiedNativeIdentity,
  ) -> Option<DeferredContextRevocation> {
    let mut state = self.0.state.lock().ok()?;
    let capability = state.contexts.iter().find_map(|(capability, record)| {
      (record.browser_id == browser_id && record.frame_id == frame_id
        && record.browser.is_same_browser(browser)
        && record.verified_identity.as_ref() == Some(identity)).then(|| capability.clone())
    })?;
    let record = state.contexts.remove(&capability)?;
    record.revoked.store(true, Ordering::Release);
    state.attestations.retain(|_, context| context != &capability);
    self.0.changed.notify_all();
    drop(state);
    Some(DeferredContextRevocation {
      origin: NativeContextToken {
        registry: Arc::downgrade(&self.0), capability, browser: browser.clone(),
        browser_id, frame_id: frame_id.to_owned(),
        revoked: record.revoked.clone(),
        navigation_issuance: record.navigation_issuance,
        native_identity: record.verified_identity.clone(),
        process_identity: record.process_identity.clone(),
      },
      listeners: record.listeners,
      delivery: None,
    })
  }

  pub(crate) fn revoke_browser(&self, browser: &FrameNavigationState) {
    let Ok(mut state) = self.0.state.lock() else { return };
    let mut revoked = Vec::new();
    state.contexts.retain(|_, record| {
      let keep = !record.browser.is_same_browser(browser);
      if !keep {
        record.revoked.store(true, Ordering::Release);
        revoked.append(&mut record.listeners);
      }
      keep
    });
    let State { contexts, attestations, .. } = &mut *state;
    attestations.retain(|_, context| contexts.contains_key(context));
    self.0.changed.notify_all();
    drop(state);
    notify_revocations(revoked);
  }

  /// Called off the UI thread, so a context-registration message may be handled
  /// while a document-start synchronous XHR waits for admission.
  pub(crate) fn attest(&self, browser: &FrameNavigationState, browser_id: i32, frame_id: &str, capability: &str, timeout: Duration) -> Option<String> {
    let deadline = Instant::now().checked_add(timeout)?;
    let mut state = self.0.state.lock().ok()?;
    loop {
      if let Some(record) = state.contexts.get(capability) {
        if record.browser_id != browser_id || record.frame_id != frame_id || !record.browser.is_same_browser(browser) { return None; }
        if browser.has_browser_id(browser_id) { break; }
      }
      let remaining = deadline.checked_duration_since(Instant::now())?;
      let (next, wait) = self.0.changed.wait_timeout(state, remaining).ok()?;
      state = next;
      if wait.timed_out() { return None; }
    }
    if state.attestations.len() >= 4096 { return None; }
    let handle = crate::cef_impl::trusted_init::random_capability()?;
    if state.attestations.contains_key(&handle) { return None; }
    state.attestations.insert(handle.clone(), capability.to_owned());
    Some(handle)
  }

  pub(crate) fn redeem(&self, handle: &str) -> Option<NativeContextToken> {
    let mut state = self.0.state.lock().ok()?;
    let capability = state.attestations.remove(handle)?;
    let record = state.contexts.get(&capability)?;
    if !record.browser.has_browser_id(record.browser_id) { return None; }
    Some(NativeContextToken { registry: Arc::downgrade(&self.0), capability: capability.clone(),
      browser: record.browser.clone(), browser_id: record.browser_id, frame_id: record.frame_id.clone(),
      revoked: record.revoked.clone(), navigation_issuance: record.navigation_issuance,
      native_identity: record.verified_identity.clone(), process_identity: record.process_identity.clone() })
  }
}

pub(crate) fn challenge_response(capability: &str, nonce: &str, unique_context_id: &str) -> String {
  let mut hash = Sha256::new();
  hash.update(b"seuoffice-cef-context-v1\0");
  hash.update(capability.as_bytes());
  hash.update(nonce.as_bytes());
  hash.update(unique_context_id.as_bytes());
  let mut encoded = String::with_capacity(64);
  const HEX: &[u8; 16] = b"0123456789abcdef";
  for byte in hash.finalize() {
    encoded.push(HEX[(byte >> 4) as usize] as char);
    encoded.push(HEX[(byte & 15) as usize] as char);
  }
  encoded
}

pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
  a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
  use super::*;

  fn browser() -> FrameNavigationState {
    let browser = FrameNavigationState::new();
    assert!(browser.bind_browser(7));
    browser
  }

  #[test]
  fn late_old_process_loss_cannot_revoke_successor_process() {
    let registry = ContextRegistry::default();
    let browser = browser();
    let old = "a".repeat(64);
    let successor = "b".repeat(64);
    assert!(registry.register(browser.clone(), 7, "old".into(), old.clone()));
    assert!(registry.register(browser.clone(), 7, "new".into(), successor.clone()));
    let first = crate::NativeRendererProcessIdentity::test_identity(3, 42, 1);
    let second = crate::NativeRendererProcessIdentity::test_identity(3, 42, 2);
    {
      let mut state = registry.0.state.lock().unwrap();
      for (capability, process) in [(&old, &first), (&successor, &second)] {
        let record = state.contexts.get_mut(capability).unwrap();
        record.verified_identity = Some(VerifiedNativeIdentity {
          renderer_child_id: 3, document_token: "A".repeat(32),
          context_token: "B".repeat(32), world_id: 0, registration: 1,
        });
        assert!(record.process_identity.set(process.clone()).is_ok());
      }
    }
    assert!(registry.revoke_process_document(&old, &second).is_none());
    let native_identity = VerifiedNativeIdentity {
      renderer_child_id: 3, document_token: "A".repeat(32),
      context_token: "B".repeat(32), world_id: 0, registration: 1,
    };
    let claim = registry.revoke_verified_deferred(&browser, 7, "old", &native_identity).unwrap();
    assert!(registry.revoke_process_document(&old, &first).is_none());
    assert!(registry.revoke_verified_deferred(&browser, 7, "old", &native_identity).is_none());
    claim.finish();
    let state = registry.0.state.lock().unwrap();
    assert!(state.contexts.contains_key(&successor));
  }

  #[test]
  fn only_native_registration_mints_verified_origin() {
    let registry = ContextRegistry::default();
    let browser = browser();
    assert!(browser.set_trusted_init_mode(crate::TrustedInitMode::ProtectedPreload));
    let legacy_cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "frame".into(), legacy_cap.clone()));
    let handle = registry.attest(&browser, 7, "frame", &legacy_cap, Duration::ZERO).unwrap();
    let legacy = registry.redeem(&handle).unwrap();
    assert!(legacy.is_live());
    assert!(!legacy.is_verified());
    let identity = VerifiedNativeIdentity {
      renderer_child_id: 2, document_token: "A".repeat(32),
      context_token: "B".repeat(32), world_id: 0,
      registration: 1,
    };
    let unprotected = FrameNavigationState::new();
    assert!(unprotected.bind_browser(8));
    assert!(unprotected.set_trusted_init_mode(crate::TrustedInitMode::Unprotected));
    assert!(registry.issue_navigation_intent(unprotected.clone(), 8).is_none());
    assert!(registry.register_verified(unprotected, 8, "frame".into(), identity.clone(), 1, "missing").is_none());
    let (sequence, intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    assert!(registry.register_verified(browser.clone(), 7, "frame".into(), identity.clone(), sequence + 1, &intent).is_none());
    let native = registry.register_verified(browser.clone(), 7, "frame".into(), identity.clone(), sequence, &intent).unwrap();
    assert!(!legacy.is_live());
    assert!(native.is_verified());
    let mut invalid = identity;
    invalid.world_id = 1;
    assert!(registry.register_verified(browser, 7, "frame".into(), invalid, sequence, &intent).is_none());
    assert!(native.is_verified());
  }

  #[test]
  fn ready_receipt_is_exact_once_and_revoked_with_original_context() {
    let registry = ContextRegistry::default();
    let browser = browser();
    assert!(browser.set_trusted_init_mode(crate::TrustedInitMode::ProtectedPreload));
    let (sequence, intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    let identity = VerifiedNativeIdentity {
      renderer_child_id: 2, document_token: "A".repeat(32),
      context_token: "B".repeat(32), world_id: 0, registration: sequence,
    };
    let origin = registry.register_verified(browser.clone(), 7, "frame".into(), identity.clone(), sequence, &intent).unwrap();
    assert!(origin.document_ready().is_none());
    assert!(origin.mark_document_ready());
    assert!(!origin.mark_document_ready());
    let receipt = origin.document_ready().unwrap();
    assert_eq!(receipt.origin(), &origin);
    assert!(receipt.is_live());
    assert_eq!(receipt.with_live(|| 7), Some(7));
    assert_eq!(registry.revoke_verified(&browser, 7, "frame", &identity), Some(origin.clone()));
    assert!(origin.document_ready().is_none());
    assert!(!receipt.is_live());
    assert!(receipt.with_live(|| 7).is_none());
  }

  #[test]
  fn late_release_of_old_native_context_cannot_revoke_replacement() {
    let registry = ContextRegistry::default();
    let browser = browser();
    assert!(browser.set_trusted_init_mode(crate::TrustedInitMode::ProtectedPreload));
    let first = VerifiedNativeIdentity {
      renderer_child_id: 2, document_token: "A".repeat(32),
      context_token: "B".repeat(32), world_id: 0,
      registration: 1,
    };
    let mut second = first.clone();
    second.document_token = "C".repeat(32);
    second.context_token = "D".repeat(32);
    let (first_sequence, first_intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    let old = registry.register_verified(browser.clone(), 7, "frame".into(), first.clone(), first_sequence, &first_intent).unwrap();
    let (obsolete_sequence, obsolete_intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    let (second_sequence, second_intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    second.registration = second_sequence;
    assert!(registry.register_verified(browser.clone(), 7, "frame".into(), second.clone(), obsolete_sequence, &obsolete_intent).is_none());
    assert!(registry.register_verified(browser.clone(), 7, "frame".into(), second.clone(), second_sequence, &second_intent).is_none());
    assert!(old.is_verified());
    assert_eq!(registry.revoke_verified(&browser, 7, "frame", &first), Some(old.clone()));
    let replacement = registry.register_verified(browser.clone(), 7, "frame".into(), second.clone(), second_sequence, &second_intent).unwrap();
    assert!(registry.register_verified(browser.clone(), 7, "frame".into(), second.clone(), second_sequence, &second_intent).is_none());
    assert!(registry.verified_token(&browser, 7, "frame", &first).is_none());
    assert_eq!(registry.verified_token(&browser, 7, "frame", &second), Some(replacement.clone()));
    assert!(registry.verified_token(&browser, 7, "other-frame", &second).is_none());
    assert!(!registry.register(browser.clone(), 7, "frame".into(), "e".repeat(64)));
    assert!(!old.is_verified());
    assert!(registry.revoke_verified(&browser, 7, "frame", &first).is_none());
    assert!(replacement.is_verified());
    assert_eq!(registry.revoke_verified(&browser, 7, "frame", &second), Some(replacement.clone()));
    assert!(!replacement.is_verified());
  }

  #[test]
  fn deferred_native_revocation_blocks_effect_before_notifying() {
    let registry = ContextRegistry::default();
    let browser = browser();
    assert!(browser.set_trusted_init_mode(crate::TrustedInitMode::ProtectedPreload));
    let identity = VerifiedNativeIdentity {
      renderer_child_id: 2, document_token: "A".repeat(32),
      context_token: "B".repeat(32), world_id: 0,
      registration: 1,
    };
    let (sequence, intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    let token = registry.register_verified(browser.clone(), 7, "frame".into(), identity.clone(), sequence, &intent).unwrap();
    let notified = Arc::new(AtomicBool::new(false));
    let flag = notified.clone();
    let _subscription = token.on_revoke(move || flag.store(true, Ordering::Release)).unwrap();
    let deferred = registry.revoke_verified_deferred(&browser, 7, "frame", &identity).unwrap();
    assert!(Arc::ptr_eq(&token.process_identity, &deferred.origin.process_identity));
    assert!(token.with_live(|| ()).is_none());
    assert!(token.is_revoked());
    assert!(!notified.load(Ordering::Acquire));
    let mut replacement_identity = identity.clone();
    replacement_identity.context_token = "C".repeat(32);
    let (next_sequence, next_intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    replacement_identity.registration = next_sequence;
    let replacement = registry.register_verified(browser, 7, "frame".into(), replacement_identity, next_sequence, &next_intent).unwrap();
    assert!(!replacement.is_revoked());
    assert!(replacement.is_verified());
    assert_eq!(deferred.finish(), token);
    assert!(notified.load(Ordering::Acquire));
  }

  #[test]
  fn intent_replay_and_late_cancel_cannot_claim_a_successor() {
    let registry = ContextRegistry::default();
    let browser = browser();
    assert!(browser.set_trusted_init_mode(crate::TrustedInitMode::ProtectedInvoke));
    let (first_sequence, old) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    let (second_sequence, current) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    assert!(second_sequence > first_sequence);
    registry.cancel_navigation_intent(&browser, &old);
    let identity = VerifiedNativeIdentity {
      renderer_child_id: 2, document_token: "A".repeat(32),
      context_token: "B".repeat(32), world_id: 0,
      registration: second_sequence,
    };
    assert!(registry.register_verified(browser.clone(), 7, "frame".into(), identity.clone(), first_sequence, &old).is_none());
    let token = registry.register_verified(browser.clone(), 7, "frame".into(), identity.clone(), second_sequence, &current).unwrap();
    assert!(registry.register_verified(browser.clone(), 7, "frame".into(), identity, second_sequence, &current).is_none());
    browser.close();
    assert!(!token.is_verified());
    assert!(registry.issue_navigation_intent(browser, 7).is_none());
  }

  #[test]
  fn retained_context_reregistration_never_revives_old_callback_identity() {
    let registry = ContextRegistry::default();
    let browser = browser();
    assert!(browser.set_trusted_init_mode(crate::TrustedInitMode::ProtectedPreload));
    let (first, first_intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    let old_identity = VerifiedNativeIdentity {
      renderer_child_id: 2, document_token: "A".repeat(32), context_token: "B".repeat(32),
      world_id: 0, registration: first,
    };
    let old = registry.register_verified(browser.clone(), 7, "frame".into(), old_identity.clone(), first, &first_intent).unwrap();
    registry.revoke_verified(&browser, 7, "frame", &old_identity).unwrap();
    let (second, second_intent) = registry.issue_navigation_intent(browser.clone(), 7).unwrap();
    let fresh_identity = VerifiedNativeIdentity { registration: second, ..old_identity.clone() };
    let fresh = registry.register_verified(browser.clone(), 7, "frame".into(), fresh_identity.clone(), second, &second_intent).unwrap();
    assert!(!old.is_verified());
    assert!(registry.verified_token(&browser, 7, "frame", &old_identity).is_none());
    assert!(registry.revoke_verified(&browser, 7, "frame", &old_identity).is_none());
    assert_eq!(registry.verified_token(&browser, 7, "frame", &fresh_identity), Some(fresh));
  }

  #[test]
  fn challenge_is_bound_to_nonce_context_and_consumed_once() {
    let registry = ContextRegistry::default();
    let browser = browser();
    let cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "cef-frame".into(), cap.clone()));
    let nonce = registry.challenge(&cap, "cdp-context-a").unwrap();
    let proof = challenge_response(&cap, &nonce, "cdp-context-a");
    assert!(!registry.confirm(&cap, &nonce, "cdp-context-b", &proof));
    assert!(!registry.confirm(&cap, &nonce, "cdp-context-a", &proof));
    let nonce = registry.challenge(&cap, "cdp-context-a").unwrap();
    let proof = challenge_response(&cap, &nonce, "cdp-context-a");
    assert!(registry.confirm(&cap, &nonce, "cdp-context-a", &proof));
    assert!(!registry.confirm(&cap, &nonce, "cdp-context-a", &proof));
  }

  #[test]
  fn replacement_revokes_old_tokens_and_one_shot_attestations() {
    let registry = ContextRegistry::default();
    let browser = browser();
    let first = "a".repeat(64);
    let second = "b".repeat(64);
    for (cap, context) in [(&first, "cdp-a"), (&second, "cdp-b")] {
      assert!(registry.register(browser.clone(), 7, "frame".into(), cap.clone()));
      let nonce = registry.challenge(cap, context).unwrap();
      assert!(registry.confirm(cap, &nonce, context, &challenge_response(cap, &nonce, context)));
      if cap == &first {
        let handle = registry.attest(&browser, 7, "frame", cap, Duration::ZERO).unwrap();
        let token = registry.redeem(&handle).unwrap();
        assert!(token.is_live());
        assert!(registry.redeem(&handle).is_none());
      }
    }
    assert!(registry.attest(&browser, 7, "frame", &first, Duration::ZERO).is_none());
    let handle = registry.attest(&browser, 7, "frame", &second, Duration::ZERO).unwrap();
    let token = registry.redeem(&handle).unwrap();
    registry.revoke(7, "frame", &second);
    assert!(!token.is_live());
  }

  #[test]
  fn subscription_is_one_shot_reentrant_and_drop_unsubscribes() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NativeContextRevocation>();
    let registry = ContextRegistry::default();
    let browser = browser();
    let cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "frame".into(), cap.clone()));
    let handle = registry.attest(&browser, 7, "frame", &cap, Duration::ZERO).unwrap();
    let token = registry.redeem(&handle).unwrap();
    let called = Arc::new(AtomicUsize::new(0));
    let count = called.clone();
    let nested_registry = registry.clone();
    let nested_browser = browser.clone();
    let subscription = token.on_revoke(move || {
      count.fetch_add(1, Ordering::SeqCst);
      nested_registry.revoke_browser(&nested_browser);
    }).unwrap();
    let cancelled = called.clone();
    drop(token.on_revoke(move || { cancelled.fetch_add(100, Ordering::SeqCst); }).unwrap());
    registry.revoke(7, "frame", &cap);
    registry.revoke(7, "frame", &cap);
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert!(token.on_revoke(|| {}).is_none());
    drop(subscription);
  }

  #[test]
  fn replacement_notifies_old_owner_and_contended_subscription_fails_closed() {
    let registry = ContextRegistry::default();
    let browser = browser();
    let cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "frame".into(), cap.clone()));
    let handle = registry.attest(&browser, 7, "frame", &cap, Duration::ZERO).unwrap();
    let token = registry.redeem(&handle).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let callback_count = count.clone();
    let _subscription = token.on_revoke(move || { callback_count.fetch_add(1, Ordering::SeqCst); }).unwrap();
    let guard = registry.0.state.lock().unwrap();
    assert!(token.on_revoke(|| {}).is_none());
    drop(guard);
    assert!(registry.register(browser, 7, "frame".into(), "b".repeat(64)));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(token.on_revoke(|| {}).is_none());
  }

  #[test]
  fn browser_close_notifies_and_refuses_late_registration() {
    let browser = browser();
    let registry = browser.native_contexts.clone();
    let cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "frame".into(), cap.clone()));
    let handle = registry.attest(&browser, 7, "frame", &cap, Duration::ZERO).unwrap();
    let token = registry.redeem(&handle).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let callback_count = count.clone();
    let _subscription = token.on_revoke(move || { callback_count.fetch_add(1, Ordering::SeqCst); }).unwrap();
    browser.close();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(token.on_revoke(|| {}).is_none());
    assert!(!registry.register(browser, 7, "frame".into(), "b".repeat(64)));
  }

  #[test]
  fn navigation_revokes_the_captured_context_subscription() {
    let browser = browser();
    let registry = browser.native_contexts.clone();
    let cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "frame".into(), cap.clone()));
    let handle = registry.attest(&browser, 7, "frame", &cap, Duration::ZERO).unwrap();
    let token = registry.redeem(&handle).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let callback_count = count.clone();
    let _subscription = token.on_revoke(move || { callback_count.fetch_add(1, Ordering::SeqCst); }).unwrap();
    browser.on_protocol_navigation(true);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(token.is_live());
    browser.on_protocol_navigation(false);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(token.on_revoke(|| {}).is_none());
  }

  #[test]
  fn concurrent_subscribe_or_revoke_never_loses_owner_loss() {
    let registry = ContextRegistry::default();
    let browser = browser();
    let cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "frame".into(), cap.clone()));
    let handle = registry.attest(&browser, 7, "frame", &cap, Duration::ZERO).unwrap();
    let token = registry.redeem(&handle).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = barrier.clone();
    let worker_registry = registry.clone();
    let worker_cap = cap.clone();
    let worker = std::thread::spawn(move || {
      worker_barrier.wait();
      worker_registry.revoke(7, "frame", &worker_cap);
    });
    let losses = Arc::new(AtomicUsize::new(0));
    let callback_losses = losses.clone();
    barrier.wait();
    let subscription = token.on_revoke(move || { callback_losses.fetch_add(1, Ordering::SeqCst); });
    if subscription.is_none() { losses.fetch_add(1, Ordering::SeqCst); }
    worker.join().unwrap();
    assert_eq!(losses.load(Ordering::SeqCst), 1);
    drop(subscription);
  }

  #[test]
  fn panicking_gate_revokes_and_notifies_before_resuming_unwind() {
    let registry = ContextRegistry::default();
    let browser = browser();
    let cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "frame".into(), cap.clone()));
    let handle = registry.attest(&browser, 7, "frame", &cap, Duration::ZERO).unwrap();
    let token = registry.redeem(&handle).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let callback_count = count.clone();
    let _subscription = token.on_revoke(move || { callback_count.fetch_add(1, Ordering::SeqCst); }).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| token.with_live(|| panic!("gate failed"))));
    assert!(result.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(!token.is_live());
  }

  fn pair_token(registry: &ContextRegistry, frame: &str, capability: &str) -> NativeContextToken {
    let browser = browser();
    assert!(registry.register(browser.clone(), 7, frame.into(), capability.into()));
    let handle = registry.attest(&browser, 7, frame, capability, Duration::ZERO).unwrap();
    registry.redeem(&handle).unwrap()
  }

  #[test]
  fn pair_gate_handles_shared_registry_and_rejects_either_revoked_owner() {
    let registry = ContextRegistry::default();
    let first = pair_token(&registry, "a", &"a".repeat(64));
    let second = pair_token(&registry, "b", &"b".repeat(64));
    assert_eq!(first.with_live_pair(&first, || 1), Some(1));
    assert_eq!(first.with_live_pair(&second, || 2), Some(2));
    assert_eq!(second.with_live_pair(&first, || 3), Some(3));
    registry.revoke(7, "a", &"a".repeat(64));
    assert!(first.is_revoked());
    assert!(first.with_live_pair(&second, || panic!("stale owner admitted")).is_none());
    assert!(second.with_live_pair(&first, || panic!("stale owner admitted")).is_none());
    assert!(second.is_live());
  }

  #[test]
  fn pair_gate_panic_revokes_both_registries_before_unlocked_loss_delivery() {
    let first_registry = ContextRegistry::default();
    let second_registry = ContextRegistry::default();
    let first = pair_token(&first_registry, "a", &"a".repeat(64));
    let second = pair_token(&second_registry, "b", &"b".repeat(64));
    let losses = Arc::new(AtomicUsize::new(0));
    let subscriptions: Vec<_> = [&first, &second].into_iter().map(|token| {
      let first_registry = first_registry.clone();
      let second_registry = second_registry.clone();
      let losses = losses.clone();
      token.on_revoke(move || {
        assert!(first_registry.0.state.try_lock().is_ok());
        assert!(second_registry.0.state.try_lock().is_ok());
        losses.fetch_add(1, Ordering::SeqCst);
      }).unwrap()
    }).collect();
    assert_eq!(first.with_live_pair(&second, || 1), Some(1));
    assert_eq!(second.with_live_pair(&first, || 2), Some(2));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
      second.with_live_pair(&first, || panic!("pair commit failed"))));
    assert!(panic.is_err());
    assert!(first.is_revoked() && second.is_revoked());
    assert!(!first.is_live() && !second.is_live());
    assert_eq!(losses.load(Ordering::SeqCst), 2);
    drop(subscriptions);
  }
}

#[cfg(test)]
mod admission_tests {
  use super::*;

  #[test]
  fn ipc_admission_does_not_wait_for_cdp_binding() {
    let browser = FrameNavigationState::new();
    assert!(browser.bind_browser(7));
    let registry = ContextRegistry::default();
    let cap = "a".repeat(64);
    assert!(registry.register(browser.clone(), 7, "frame".into(), cap.clone()));
    let handle = registry.attest(&browser, 7, "frame", &cap, Duration::ZERO).unwrap();
    let token = registry.redeem(&handle).unwrap();
    assert!(token.is_live());
    assert!(token.eval_context().is_none());
    let nonce = registry.challenge(&cap, "cdp").unwrap();
    assert!(registry.confirm(&cap, &nonce, "cdp", &challenge_response(&cap, &nonce, "cdp")));
    assert!(token.eval_context().unwrap().is_live());
  }
}
