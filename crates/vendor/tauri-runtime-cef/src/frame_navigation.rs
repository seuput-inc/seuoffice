// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Native document generations shared by every CEF webview.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use std::sync::{Arc, Mutex};

const MAX_NATIVE_FRAMES: usize = 256;
const MAX_NATIVE_FRAME_ID_BYTES: usize = 512;

struct ContextSeed(String);

impl std::fmt::Debug for ContextSeed {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("ContextSeed([redacted])")
  }
}

#[derive(Debug, Default)]
struct NativeFrameState {
  browser_id: Option<i32>,
  context_seed: Option<ContextSeed>,
  trusted_init_mode: Option<crate::TrustedInitMode>,
  generation: u64,
  navigation_generation: u64,
  issued_navigation: u64,
  exhausted: bool,
  loading: bool,
  observed_load_state: bool,
  main_frame: Option<String>,
  frames: BTreeMap<String, bool>,
}

/// Read-only navigation state for one exact native CEF browser lifetime. The CEF UI thread
/// advances it before input submission can be admitted on that same thread.
/// No lock is held while calling CEF or awaiting a renderer response.
#[derive(Clone, Debug)]
pub struct FrameNavigationState {
  pub(crate) trusted_activation: crate::cef_impl::trusted_activation::TrustedActivation,
  pub(crate) native_contexts: crate::native_context::ContextRegistry,
  state: Arc<Mutex<NativeFrameState>>,
  pub(crate) browser_close: crate::NativeBrowserClose,
  pub(crate) native_reload: crate::native_reload::NativeReloadTracker,
  pub(crate) observed_navigation: crate::native_observed_navigation::Registry,
}

/// Opaque process-local proof of an observed native browser/document lifetime.
/// Compare it with `WebviewSnapshot::document` in the final UI-thread callback
/// before an effect. A token alone does not authorize an account or profile.
#[derive(Clone)]
pub struct NativeDocumentToken {
  state: Arc<Mutex<NativeFrameState>>,
  generation: u64,
}

impl PartialEq for NativeDocumentToken {
  fn eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.state, &other.state) && self.generation == other.generation
  }
}
impl Eq for NativeDocumentToken {}

impl std::fmt::Debug for NativeDocumentToken {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("NativeDocumentToken")
      .finish_non_exhaustive()
  }
}

/// A native browser lifetime and its observed main-frame navigation epoch.
///
/// Available while a document loads. Unlike `NativeDocumentToken`, this is not
/// proof that a request originated in the document being loaded. CEF's
/// `on_before_browse` can miss same-document and no-network navigations, so an
/// unchanged epoch does not prove that no navigation occurred. A consumer must
/// control the browser's navigations and recheck the token before using it for a
/// document-scoped operation.
#[derive(Clone)]
pub struct NativeNavigationToken {
  state: Arc<Mutex<NativeFrameState>>,
  generation: u64,
}

impl PartialEq for NativeNavigationToken {
  fn eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.state, &other.state) && self.generation == other.generation
  }
}
impl Eq for NativeNavigationToken {}
impl std::fmt::Debug for NativeNavigationToken {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.debug_struct("NativeNavigationToken").finish_non_exhaustive()
  }
}

impl FrameNavigationState {
  pub(crate) fn new() -> Self {
    Self {
      trusted_activation: Default::default(),
      native_contexts: Default::default(),
      state: Arc::default(),
      browser_close: Default::default(),
      native_reload: Default::default(),
      observed_navigation: Default::default(),
    }
  }

  /// Identifies the same native browser lifetime independently of navigation.
  /// A replacement browser never matches, even if a caller reuses its label.
  pub fn is_same_browser(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.state, &other.state)
  }

  /// Admit a short, non-reentrant in-memory effect for this browser creation.
  /// The effect must not call CEF, wait, or re-enter any browser-state API.
  /// Browser close serializes with this gate; navigation does not revoke it.
  pub fn with_live_browser<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
    let state = self.state.lock().ok()?;
    if state.exhausted || state.browser_id.is_none() { return None; }
    Some(effect())
  }

  pub fn when_closed(&self, callback: impl FnOnce(crate::NativeBrowserCloseOutcome) + Send + 'static)
    -> Result<crate::NativeBrowserCloseSubscription, String> {
    self.browser_close.when_closed(callback)
  }

  pub(crate) fn next_navigation_issuance(&self, browser_id: i32) -> Option<u64> {
    let mut state = self.state.lock().ok()?;
    if state.exhausted || state.browser_id != Some(browser_id) { return None; }
    let next = state.issued_navigation.checked_add(1)?;
    state.issued_navigation = next;
    Some(next)
  }

  pub(crate) fn has_navigation_issuance(&self, browser_id: i32, issuance: u64) -> bool {
    self.state.lock().is_ok_and(|state|
      !state.exhausted && state.browser_id == Some(browser_id) && state.issued_navigation == issuance)
  }

  /// Redeems a browser-issued request attestation exactly once for this browser
  /// lifetime. A renderer-supplied capability is not an attestation.
  pub fn redeem_native_context(&self, headers: &http::HeaderMap) -> Option<crate::NativeContextToken> {
    let handle = headers.get(crate::NATIVE_ATTESTATION_HEADER)?.to_str().ok()?;
    let token = self.native_contexts.redeem(handle)?;
    self.is_same_browser(token.browser_state()).then_some(token)
  }

  /// Captures the observed main-navigation epoch even while a page loads.
  /// It does not establish request-to-document provenance.
  pub fn navigation(&self) -> Option<NativeNavigationToken> {
    let state = self.state.lock().ok()?;
    if state.exhausted || state.browser_id.is_none() { return None; }
    Some(NativeNavigationToken { state: Arc::clone(&self.state), generation: state.navigation_generation })
  }


  pub(crate) fn set_trusted_init_mode(&self, mode: crate::TrustedInitMode) -> bool {
    let Ok(mut state) = self.state.lock() else { return false };
    if state.exhausted || state.trusted_init_mode.is_some() { return false; }
    state.trusted_init_mode = Some(mode);
    true
  }

  pub(crate) fn trusted_init_mode(&self) -> Option<crate::TrustedInitMode> {
    let state = self.state.lock().ok()?;
    (!state.exhausted).then_some(state.trusted_init_mode).flatten()
  }
  pub(crate) fn set_context_seed(&self, seed: String) -> bool {
    let Ok(mut state) = self.state.lock() else { return false };
    if state.exhausted || state.context_seed.is_some() { return false; }
    if !self.trusted_activation.configure(seed.clone()) { return false; }
    state.context_seed = Some(ContextSeed(seed));
    true
  }

  pub(crate) fn accepts_context_seed(&self, seed: &str) -> bool {
    let Ok(state) = self.state.lock() else { return false };
    let Some(expected) = state.context_seed.as_ref() else { return false };
    if state.exhausted || expected.0.len() != seed.len() { return false; }
    expected.0.bytes().zip(seed.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
  }

  pub(crate) fn bind_browser(&self, browser_id: i32) -> bool {
    let Ok(mut state) = self.state.lock() else { return false };
    if state.exhausted || state.browser_id.is_some_and(|id| id != browser_id) { return false; }
    state.browser_id = Some(browser_id);
    true
  }

  pub(crate) fn bound_browser_id(&self) -> Option<i32> {
    let state = self.state.lock().ok()?;
    if state.exhausted { None } else { state.browser_id }
  }

  pub(crate) fn has_browser_id(&self, browser_id: i32) -> bool {
    self
      .state
      .lock()
      .is_ok_and(|state| !state.exhausted && state.browser_id == Some(browser_id))
  }

  pub(crate) fn try_has_browser_id(&self, browser_id: i32) -> bool {
    self.state.try_lock().is_ok_and(|state| !state.exhausted && state.browser_id == Some(browser_id))
  }

  pub(crate) fn invalidate_browser_lifetime(&self) {
    if let Ok(mut state) = self.state.lock() { state.exhausted = true; }
    self.observed_navigation.close();
  }

  pub(crate) fn close(&self) {
    self.native_reload.closed();
    self.observed_navigation.close();
    if let Ok(mut state) = self.state.lock() {
      state.exhausted = true;
    }
    self.native_contexts.cancel_browser_intents(self);
    self.native_contexts.remove_invocation_target(self);
    self.native_contexts.revoke_browser(self);
    self.trusted_activation.fail();
  }

  pub(crate) fn observe_document(&self, browser: &cef::Browser) -> Option<NativeDocumentToken> {
    use cef::{ImplBrowser, ImplFrame};
    let generation = self.ready_generation()?;
    if browser.is_valid() == 0 || browser.frame_count() > MAX_NATIVE_FRAMES {
      return None;
    }
    let mut identifiers = cef::CefStringList::new();
    browser.frame_identifiers(Some(&mut identifiers));
    // CEF keeps listing a frame parked in the back/forward cache, detached from
    // any renderer - Chrome style parks the internal initial document there on
    // the first navigation. Tracking drops a frame on detach, so only attached
    // frames are compared; a restored frame reattaches, advancing the generation.
    let attached = identifiers
      .into_iter()
      .filter(|id| {
        browser
          .frame_by_identifier(Some(&cef::CefString::from(id.as_str())))
          .is_some_and(|frame| frame.is_valid() != 0)
      })
      .collect();
    self
      .admits_native_frames(
        generation,
        browser.identifier(),
        browser.is_loading() != 0,
        &attached,
      )
      .then(|| NativeDocumentToken {
        state: Arc::clone(&self.state),
        generation,
      })
  }

  /// Captures an observed document only when every known frame is attached and
  /// native load completion has been observed. Compare with the final native
  /// snapshot before dispatch; this read does not query current CEF frame IDs.
  pub fn document(&self) -> Option<NativeDocumentToken> {
    self
      .ready_generation()
      .map(|generation| NativeDocumentToken {
        state: Arc::clone(&self.state),
        generation,
      })
  }

  fn ready_generation(&self) -> Option<u64> {
    let state = self.state.lock().ok()?;
    let ready = Self::ready(&state);
    if !ready {
      log::debug!(
        "native document unavailable: exhausted={}, load_observed={}, loading={}, frames={}, attached={}, main_attached={}",
        state.exhausted,
        state.observed_load_state,
        state.loading,
        state.frames.len(),
        state.frames.values().filter(|attached| **attached).count(),
        state
          .main_frame
          .as_ref()
          .is_some_and(|id| state.frames.get(id) == Some(&true)),
      );
    }
    ready.then_some(state.generation)
  }

  fn ready(state: &NativeFrameState) -> bool {
    !state.exhausted
      && state.observed_load_state
      && !state.loading
      && !state.frames.is_empty()
      // Exceeding the tracked frame cap fails admission closed for as long as
      // it lasts, matching `observe_document`'s native `frame_count` guard.
      // Frames keep being tracked exactly, so draining below the cap recovers.
      && state.frames.len() <= MAX_NATIVE_FRAMES
      && state.frames.values().all(|attached| *attached)
      && state
        .main_frame
        .as_ref()
        .is_some_and(|id| state.frames.get(id) == Some(&true))
  }

  // CDP observes same-document, BFCache and no-network starts that CEF's
  // on_before_browse does not necessarily report.
  pub(crate) fn on_protocol_navigation(&self, same_document: bool) {
    let Ok(mut state) = self.state.lock() else { return };
    if state.exhausted { return; }
    let (Some(generation), Some(navigation)) =
      (state.generation.checked_add(1), state.navigation_generation.checked_add(1))
    else { state.exhausted = true; return };
    state.generation = generation;
    state.navigation_generation = navigation;
    if !same_document { state.loading = true; }
  }

  pub(crate) fn on_frame_event(&self, event: &crate::FrameEvent) {
    // Document revocation comes from the RFH-bound native pre-mutation hook.
    // Delayed CEF/CDP loading events cannot revoke a successor registration.
    self.apply(
      event.browser_id,
      &event.frame_id,
      event.is_main,
      &event.kind,
    );
  }

  fn apply(&self, browser_id: i32, frame_id: &str, is_main: bool, kind: &crate::FrameEventKind) {
    use crate::FrameEventKind;

    let Ok(mut state) = self.state.lock() else {
      return;
    };
    if state.exhausted {
      return;
    }
    // Runtime-managed popups can inherit an opener's event handlers. Their
    // browser identity never advances or grants authority over the opener.
    if state
      .browser_id
      .is_some_and(|expected| expected != browser_id)
    {
      return;
    }
    // Bind the native browser identity before any rejection below can latch
    // exhaustion. IPC admission and event delivery are gated on this identity,
    // so an unexpected first notification must never leave it unrecorded.
    state.browser_id = Some(browser_id);
    if matches!(kind, FrameEventKind::RendererProcessIdentified { .. }
      | FrameEventKind::NativeContextRegistered { .. }
      | FrameEventKind::NativeContextRevoked { .. }
      | FrameEventKind::NativeDocumentReady { .. }
      | FrameEventKind::NativeRendererTerminated { .. }
      | FrameEventKind::RendererTerminated
      | FrameEventKind::FirstVisuallyNonEmptyLayout { .. }) {
      return;
    }
    let browser_wide = matches!(
      kind,
      FrameEventKind::MainFrameChanged
        | FrameEventKind::LoadingStateChanged { .. }
        | FrameEventKind::RendererTerminated
    );
    if frame_id.is_empty() && !browser_wide || frame_id.len() > MAX_NATIVE_FRAME_ID_BYTES {
      log::warn!(
        "native frame tracking exhausted: invalid identifier, empty={}, length={}, main={}, teardown={}",
        frame_id.is_empty(),
        frame_id.len(),
        is_main,
        matches!(kind, FrameEventKind::Detached | FrameEventKind::Destroyed),
      );
      state.exhausted = true;
      return;
    }
    let Some(generation) = state.generation.checked_add(1) else {
      state.exhausted = true;
      return;
    };
    state.generation = generation;
    if matches!(kind, FrameEventKind::RendererTerminated)
      || is_main && matches!(kind, FrameEventKind::NavigationStarted { .. } | FrameEventKind::NavigationFailed { .. })
    {
      let Some(next) = state.navigation_generation.checked_add(1) else {
        state.exhausted = true;
        return;
      };
      state.navigation_generation = next;
    }
    match kind {
      FrameEventKind::Created => {
        state.frames.insert(frame_id.to_string(), false);
      }
      FrameEventKind::Attached => {
        state.frames.insert(frame_id.to_string(), true);
      }
      FrameEventKind::Detached | FrameEventKind::Destroyed => {
        state.frames.remove(frame_id);
        if state.main_frame.as_deref() == Some(frame_id) {
          state.main_frame = None;
        }
      }
      FrameEventKind::MainFrameChanged => {
        state.main_frame = is_main.then(|| frame_id.to_string());
      }
      FrameEventKind::NavigationStarted { .. } => {
        state.loading = true;
      }
      FrameEventKind::LoadingStateChanged { is_loading } => {
        state.observed_load_state = true;
        state.loading = *is_loading;
      }
      FrameEventKind::DocumentCommitted { .. }
      | FrameEventKind::NavigationFailed { .. }
      | FrameEventKind::AddressChanged { .. }
      | FrameEventKind::RendererProcessIdentified { .. }
      | FrameEventKind::NativeContextRegistered { .. }
      | FrameEventKind::NativeContextRevoked { .. }
      | FrameEventKind::NativeDocumentReady { .. }
      | FrameEventKind::NativeRendererTerminated { .. }
      | FrameEventKind::FirstVisuallyNonEmptyLayout { .. } => {}
      FrameEventKind::RendererTerminated => {
        state.frames.clear();
        state.main_frame = None;
        state.loading = true;
        state.observed_load_state = false;
      }
    }
  }

  /// Final native admission compares the exact current CEF frame identities,
  /// not just their count. A replacement cannot reuse a document capability.
  pub(crate) fn admits_native_frames(
    &self,
    expected: u64,
    browser_id: i32,
    is_loading: bool,
    frame_ids: &BTreeSet<String>,
  ) -> bool {
    let Ok(state) = self.state.lock() else {
      return false;
    };
    Self::ready(&state)
      && state.generation == expected
      && state.browser_id == Some(browser_id)
      && !is_loading
      && state.frames.keys().eq(frame_ids.iter())
  }
}

#[cfg(test)]
mod tests {
  use crate::FrameEventKind as Event;

  use super::*;

  fn apply(barrier: &FrameNavigationState, frame: &str, kind: Event) {
    barrier.apply(1, frame, frame == "main", &kind);
  }

  fn ready_main() -> FrameNavigationState {
    let barrier = FrameNavigationState::new();
    apply(&barrier, "main", Event::Created);
    apply(&barrier, "main", Event::MainFrameChanged);
    apply(&barrier, "main", Event::Attached);
    apply(
      &barrier,
      "main",
      Event::LoadingStateChanged { is_loading: false },
    );
    barrier
  }

  fn admits(barrier: &FrameNavigationState, generation: u64, frames: &[&str]) -> bool {
    barrier.admits_native_frames(
      generation,
      1,
      false,
      &frames.iter().map(|id| id.to_string()).collect(),
    )
  }

  #[test]
  fn navigation_epoch_exists_during_loading_and_is_revoked_on_new_navigation_and_close() {
    let state = FrameNavigationState::new();
    assert!(state.navigation().is_none());
    assert!(state.bind_browser(1));
    let before = state.navigation().unwrap();
    let url = url::Url::parse("https://example.test/doc").unwrap();
    apply(&state, "main", Event::NavigationStarted { url: url.clone() });
    let loading = state.navigation().unwrap();
    assert_ne!(before, loading);
    apply(&state, "main", Event::MainFrameChanged);
    assert_eq!(state.navigation(), Some(loading.clone()));
    apply(&state, "main", Event::DocumentCommitted { url: url.clone() });
    assert_eq!(state.navigation(), Some(loading.clone()));
    assert!(state.document().is_none());
    apply(&state, "main", Event::NavigationFailed { url });
    assert_ne!(state.navigation(), Some(loading));
    state.close();
    assert!(state.navigation().is_none());
  }

  #[test]
  fn closed_browser_does_not_accept_late_identity_events() {
    let state = FrameNavigationState::new();
    assert!(state.bind_browser(1));
    assert!(state.has_browser_id(1));
    assert!(!state.has_browser_id(2));
    state.close();
    assert!(!state.has_browser_id(1));
    assert!(!state.bind_browser(1));
  }

  #[test]
  fn protocol_navigation_revokes_prior_tokens_without_before_browse() {
    let state = ready_main();
    let document = state.document().unwrap();
    let navigation = state.navigation().unwrap();
    state.on_protocol_navigation(true);
    assert_ne!(state.document(), Some(document));
    assert_ne!(state.navigation(), Some(navigation));
    let document = state.document().unwrap();
    state.on_protocol_navigation(false);
    assert!(state.document().is_none());
    apply(&state, "main", Event::LoadingStateChanged { is_loading: false });
    assert_ne!(state.document(), Some(document));
  }

  #[test]
  fn renderer_identity_observation_does_not_revoke_document() {
    let barrier = ready_main();
    let generation = barrier.ready_generation().unwrap();
    apply(&barrier, "main", Event::RendererProcessIdentified { pid: 42 });
    assert_eq!(barrier.ready_generation(), Some(generation));
    assert!(admits(&barrier, generation, &["main"]));
  }

  #[test]
  fn child_navigation_replacement_and_detach_revoke_prior_documents() {
    let barrier = ready_main();
    let main_generation = barrier.ready_generation().unwrap();
    assert!(admits(&barrier, main_generation, &["main"]));
    apply(&barrier, "child-a", Event::Created);
    assert!(barrier.ready_generation().is_none());
    apply(&barrier, "child-a", Event::Attached);
    let child_generation = barrier.ready_generation().unwrap();
    assert!(!admits(&barrier, main_generation, &["main", "child-a"]));
    assert!(admits(&barrier, child_generation, &["main", "child-a"]));
    let url = url::Url::parse("https://example.test/frame").unwrap();
    apply(
      &barrier,
      "child-a",
      Event::NavigationStarted { url: url.clone() },
    );
    assert!(barrier.ready_generation().is_none());
    apply(&barrier, "child-a", Event::DocumentCommitted { url });
    assert!(barrier.ready_generation().is_none());
    apply(
      &barrier,
      "main",
      Event::LoadingStateChanged { is_loading: false },
    );
    assert!(!admits(&barrier, child_generation, &["main", "child-a"]));
    let loaded_generation = barrier.ready_generation().unwrap();
    assert!(admits(&barrier, loaded_generation, &["main", "child-a"]));
    // Even an equal native frame count must reject different identities.
    assert!(!admits(&barrier, loaded_generation, &["main", "child-b"]));
    apply(&barrier, "child-a", Event::Detached);
    apply(&barrier, "child-b", Event::Created);
    apply(&barrier, "child-b", Event::Attached);
    assert!(!admits(&barrier, loaded_generation, &["main", "child-b"]));
    let replacement_generation = barrier.ready_generation().unwrap();
    assert!(admits(
      &barrier,
      replacement_generation,
      &["main", "child-b"]
    ));
    apply(&barrier, "child-b", Event::Detached);
    assert!(!admits(&barrier, replacement_generation, &["main"]));
    assert!(admits(
      &barrier,
      barrier.ready_generation().unwrap(),
      &["main"]
    ));
  }

  #[test]
  fn pending_cancelled_navigation_waits_for_native_loading_completion() {
    let barrier = ready_main();
    let url = url::Url::parse("https://example.test/repeated").unwrap();
    apply(
      &barrier,
      "main",
      Event::NavigationStarted { url: url.clone() },
    );
    apply(
      &barrier,
      "main",
      Event::NavigationStarted { url: url.clone() },
    );
    apply(&barrier, "main", Event::NavigationFailed { url });
    assert!(barrier.ready_generation().is_none());
    apply(
      &barrier,
      "main",
      Event::LoadingStateChanged { is_loading: false },
    );
    assert!(barrier.ready_generation().is_some());
  }

  #[test]
  fn foreign_browser_unobserved_frames_and_exhaustion_fail_closed() {
    let barrier = ready_main();
    let generation = barrier.ready_generation().unwrap();
    barrier.apply(2, "popup", true, &Event::Created);
    assert_eq!(barrier.ready_generation(), Some(generation));
    assert!(!barrier.admits_native_frames(
      generation,
      2,
      false,
      &BTreeSet::from(["main".to_string()])
    ));
    assert!(!admits(&barrier, generation, &["main", "unobserved"]));
    barrier.state.lock().unwrap().generation = u64::MAX;
    apply(&barrier, "main", Event::Attached);
    assert!(barrier.ready_generation().is_none());
    apply(
      &barrier,
      "main",
      Event::LoadingStateChanged { is_loading: false },
    );
    assert!(barrier.ready_generation().is_none());
  }
  #[test]
  fn missing_main_frame_invalidates_generation_but_stock_termination_cannot_relabel_it() {
    let barrier = ready_main();
    let generation = barrier.ready_generation().unwrap();
    barrier.apply(1, "", false, &Event::MainFrameChanged);
    assert!(barrier.ready_generation().is_none());
    apply(&barrier, "main", Event::MainFrameChanged);
    assert_ne!(barrier.ready_generation().unwrap(), generation);
    let current = barrier.ready_generation().unwrap();
    barrier.apply(1, "", false, &Event::RendererTerminated);
    assert_eq!(barrier.ready_generation(), Some(current));
    apply(&barrier, "replacement", Event::Created);
    apply(&barrier, "replacement", Event::Attached);
    barrier.apply(1, "replacement", true, &Event::MainFrameChanged);
    assert_ne!(barrier.ready_generation().unwrap(), generation);
  }
  #[test]
  fn document_tokens_cannot_cross_native_lifetimes_or_navigation() {
    let first = ready_main();
    let second = ready_main();
    assert_eq!(first.ready_generation(), second.ready_generation());
    assert!(first.is_same_browser(&first.clone()));
    assert!(!first.is_same_browser(&second));
    let before = first.document().unwrap();
    assert_eq!(before, first.clone().document().unwrap());
    assert_ne!(before, second.document().unwrap());
    apply(&first, "child", Event::Created);
    assert!(first.document().is_none());
    apply(&first, "child", Event::Attached);
    assert_ne!(before, first.document().unwrap());
  }

  #[test]
  fn browser_gate_survives_navigation_and_rejects_exact_close() {
    let original = ready_main();
    let replacement = ready_main();
    assert_eq!(original.with_live_browser(|| 1), Some(1));
    original.on_protocol_navigation(false);
    assert_eq!(original.with_live_browser(|| 2), Some(2));
    original.invalidate_browser_lifetime();
    assert_eq!(original.with_live_browser(|| 3), None);
    assert_eq!(replacement.with_live_browser(|| 4), Some(4));
  }
  #[test]
  fn a_rejected_first_event_still_records_the_native_browser_identity() {
    // `AddressChanged`/`NavigationFailed` tolerate a missing native frame, so a
    // browser's very first notification can carry an empty identifier.
    let url = url::Url::parse("https://example.test/frameless").unwrap();
    let barrier = FrameNavigationState::new();
    barrier.apply(1, "", false, &Event::AddressChanged { url });
    // Document admission still fails closed, ...
    assert!(barrier.document().is_none());
    // ... but IPC and event delivery stay bound to this exact browser.
    assert!(barrier.has_browser_id(1));
    assert!(!barrier.has_browser_id(2));
  }
  #[test]
  fn a_transient_frame_count_overflow_recovers_once_frames_drain() {
    let barrier = ready_main();
    for index in 0..MAX_NATIVE_FRAMES {
      apply(&barrier, &format!("child-{index}"), Event::Created);
      apply(&barrier, &format!("child-{index}"), Event::Attached);
    }
    assert!(barrier.ready_generation().is_none());
    apply(&barrier, "child-0", Event::Detached);
    assert!(barrier.ready_generation().is_some());
    assert!(barrier.has_browser_id(1));
  }
}
