// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex};

use cef::*;

const ACTIVATE: &str = "seuoffice:activate-trusted-context";
pub(crate) const ACK: &str = "seuoffice:activate-trusted-context-ack";

struct Ready {
  browser: Browser,
  url: String,
  gate: Arc<crate::navigation_event::NavigationGate>,
  state: crate::FrameNavigationState,
  explicit_navigation: bool,
}

#[derive(Default)]
struct State {
  seed: Option<String>,
  unprotected_explicit: bool,
  inert_loaded: bool,
  owner_ready: bool,
  ready: Option<Ready>,
  awaiting: bool,
  finished: bool,
}

#[derive(Clone, Default)]
pub(crate) struct TrustedActivation(Arc<Mutex<State>>);

impl std::fmt::Debug for TrustedActivation {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("TrustedActivation([redacted])")
  }
}

impl TrustedActivation {
  pub(crate) fn configure(&self, seed: String) -> bool {
    let Ok(mut state) = self.0.lock() else { return false };
    if state.seed.is_some() || state.unprotected_explicit || state.finished { return false; }
    state.seed = Some(seed);
    true
  }

  pub(crate) fn configure_unprotected_explicit(&self) -> bool {
    let Ok(mut state) = self.0.lock() else { return false };
    if state.seed.is_some() || state.unprotected_explicit || state.finished { return false; }
    state.unprotected_explicit = true;
    true
  }

  pub(crate) fn inert_loaded(&self) {
    if let Ok(mut state) = self.0.lock() {
      if (state.seed.is_none() && !state.unprotected_explicit) || state.finished { return; }
      state.inert_loaded = true;
    }
    self.try_activate();
  }

  pub(crate) fn owner_ready(&self) -> bool {
    let Ok(mut state) = self.0.lock() else { return false };
    if (state.seed.is_none() && !state.unprotected_explicit) || state.owner_ready || state.finished { return false; }
    state.owner_ready = true;
    drop(state);
    self.try_activate();
    true
  }

  /// Returns false for an unprotected browser, whose normal initialization path remains in use.
  pub(crate) fn ready(&self, browser: Browser, url: String, gate: Arc<crate::navigation_event::NavigationGate>, native_state: crate::FrameNavigationState, explicit_navigation: bool) -> bool {
    let Ok(mut state) = self.0.lock() else { gate.fail(); return true };
    if state.seed.is_none() && !state.unprotected_explicit { return false; }
    if state.finished || state.ready.is_some() { gate.fail(); return true; }
    state.ready = Some(Ready { browser, url, gate, state: native_state, explicit_navigation });
    drop(state);
    let mut timeout = ActivationTimeout::new(self.clone());
    if post_delayed_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut timeout), 10_000) != 1 {
      self.fail();
      return true;
    }
    self.try_activate();
    true
  }

  fn try_activate(&self) {
    let Ok(mut state) = self.0.lock() else { return };
    if !state.owner_ready || !state.inert_loaded || state.awaiting || state.finished { return; }
    if state.unprotected_explicit {
      let Some(ready) = state.ready.take() else { return };
      state.finished = true;
      drop(state);
      ready.gate.arm();
      if !ready.gate.release() {
        ready.gate.fail();
        if let Some(host) = ready.browser.host() { host.close_browser(1); }
      }
      return;
    }
    let (Some(seed), Some(ready)) = (&state.seed, &state.ready) else { return };
    let seed = seed.clone();
    let browser = ready.browser.clone();
    state.awaiting = true;
    drop(state);
    let Some(frame) = browser.main_frame() else { self.fail(); return };
    let Some(mut message) = process_message_create(Some(&CefString::from(ACTIVATE))) else { self.fail(); return };
    let Some(args) = message.argument_list() else { self.fail(); return };
    if args.set_string(0, Some(&CefString::from(seed.as_str()))) == 0 { self.fail(); return; }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
    // The acknowledgement is handled by the browser client without blocking CEF's UI thread.
  }

  pub(crate) fn acknowledge(&self, browser_id: i32, seed: &str) -> bool {
    let Ok(mut state) = self.0.lock() else { return false };
    if !state.awaiting || state.finished || state.seed.as_deref() != Some(seed)
      || !state.ready.as_ref().is_some_and(|ready| ready.browser.identifier() == browser_id) {
      return false;
    }
    state.finished = true;
    state.awaiting = false;
    let Some(ready) = state.ready.take() else { return false };
    drop(state);
    if ready.explicit_navigation {
      ready.gate.release();
      return true;
    }
    let Some((issuance, intent)) = ready.state.native_contexts.issue_navigation_intent(ready.state.clone(), browser_id) else {
      ready.gate.fail();
      if let Some(host) = ready.browser.host() { host.close_browser(1); }
      return false;
    };
    ready.gate.arm();
    let Some(host) = ready.browser.host() else { ready.gate.fail(); return false };
    let mut callback = ApprovedInitialLoad::new(ready.state, ready.browser, intent.clone(), ready.gate);
    host.load_approved_main_frame_url(Some(&CefString::from(ready.url.as_str())), issuance,
      Some(&CefString::from(intent.as_str())), Some(&mut callback));
    true
  }

  pub(crate) fn fail(&self) {
    let Ok(mut state) = self.0.lock() else { return };
    if state.finished { return; }
    state.finished = true;
    let ready = state.ready.take();
    drop(state);
    if let Some(ready) = ready {
      ready.gate.fail();
      if let Some(host) = ready.browser.host() { host.close_browser(1); }
    }
  }

  fn timeout(&self) {
    let awaiting = self.0.lock().is_ok_and(|state| state.ready.is_some() && !state.finished);
    if awaiting { self.fail(); }
  }
}

wrap_task! {
  struct ActivationTimeout { activation: TrustedActivation }
  impl Task {
    fn execute(&self) { self.activation.timeout(); }
  }
}

wrap_approved_navigation_callback! {
  struct ApprovedInitialLoad {
    state: crate::FrameNavigationState,
    browser: Browser,
    intent: String,
    gate: Arc<crate::navigation_event::NavigationGate>,
  }
  impl ApprovedNavigationCallback {
    fn on_complete(&self, accepted: i32) {
      let mut task = CompleteApprovedInitialLoad::new(self.state.clone(), self.browser.clone(),
        self.intent.clone(), self.gate.clone(), accepted != 0);
      if post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task)) != 1 {
        self.gate.fail();
        self.state.native_contexts.cancel_navigation_intent(&self.state, &self.intent);
      }
    }
  }
}

wrap_task! {
  struct CompleteApprovedInitialLoad {
    state: crate::FrameNavigationState,
    browser: Browser,
    intent: String,
    gate: Arc<crate::navigation_event::NavigationGate>,
    accepted: bool,
  }
  impl Task {
    fn execute(&self) {
      if !self.state.has_browser_id(self.browser.identifier()) { self.gate.fail(); return; }
      if self.accepted {
        self.gate.release();
      } else if self.state.native_contexts.cancel_navigation_intent(&self.state, &self.intent) {
        self.gate.fail();
        if let Some(host) = self.browser.host() { host.close_browser(1); }
      }
    }
  }
}

pub(crate) fn activation_message_name() -> &'static str { ACTIVATE }

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn unprotected_explicit_requires_configuration_and_owner_once() {
    let activation = TrustedActivation::default();
    assert!(!activation.owner_ready());
    assert!(activation.configure_unprotected_explicit());
    assert!(!activation.configure("protected-seed".into()));
    activation.inert_loaded();
    assert!(activation.owner_ready());
    assert!(!activation.owner_ready());
    let state = activation.0.lock().unwrap();
    assert!(state.inert_loaded && state.owner_ready);
    assert!(state.seed.is_none() && !state.finished && state.ready.is_none());
  }

  #[test]
  fn closed_explicit_activation_cannot_be_reopened() {
    let activation = TrustedActivation::default();
    assert!(activation.configure_unprotected_explicit());
    activation.fail();
    activation.inert_loaded();
    assert!(!activation.owner_ready());
    assert!(activation.0.lock().unwrap().finished);
  }
}
