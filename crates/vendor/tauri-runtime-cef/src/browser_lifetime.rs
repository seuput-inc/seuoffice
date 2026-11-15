// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cef::BrowserHost;
use tauri_runtime::window::WindowId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
  Creating,
  Reserved,
  Attached,
  Closing { attached: bool },
  Destroying { attached: bool },
  Closed { attached: bool },
}

struct Entry {
  window_id: WindowId,
  position: Arc<Mutex<WindowId>>,
  browser_id: Option<i32>,
  state: Option<crate::FrameNavigationState>,
  host: Option<BrowserHost>,
  phase: Phase,
  close_requested: bool,
  close: crate::NativeBrowserClose,
}

#[derive(Clone, Default)]
pub(crate) struct BrowserLifetimes(Arc<Mutex<HashMap<u32, Entry>>>);

impl BrowserLifetimes {
  pub(crate) fn reserve(&self, webview_id: u32, window_id: WindowId) -> Option<crate::NativeBrowserClose> {
    self.reserve_with_close(webview_id, window_id, crate::NativeBrowserClose::default())
  }

  pub(crate) fn reserve_with_close(&self, webview_id: u32, window_id: WindowId, close: crate::NativeBrowserClose) -> Option<crate::NativeBrowserClose> {
    let mut entries = self.0.lock().ok()?;
    if entries.contains_key(&webview_id) { return None; }
    entries.insert(webview_id, Entry { window_id, position: Arc::new(Mutex::new(window_id)), browser_id: None, state: None, host: None, phase: Phase::Reserved, close_requested: false, close: close.clone() });
    Some(close)
  }

  pub(crate) fn close_handle(&self, webview_id: u32) -> Option<crate::NativeBrowserClose> {
    self.0.lock().ok()?.get(&webview_id).map(|entry| entry.close.clone())
  }

  pub(crate) fn position(&self, webview_id: u32, close: &crate::NativeBrowserClose) -> Option<Arc<Mutex<WindowId>>> {
    let entries = self.0.lock().ok()?;
    let entry = entries.get(&webview_id)?;
    entry.close.same_reservation(close).then(|| entry.position.clone())
  }

  pub(crate) fn begin(&self, webview_id: u32, window_id: WindowId) -> bool {
    let Ok(mut entries) = self.0.lock() else { return false };
    let Some(entry) = entries.get_mut(&webview_id) else { return false };
    if entry.window_id != window_id || entry.phase != Phase::Reserved { return false; }
    entry.phase = Phase::Creating;
    true
  }

  pub(crate) fn created(&self, webview_id: u32, browser_id: i32, state: crate::FrameNavigationState, host: BrowserHost) -> bool {
    let Ok(mut entries) = self.0.lock() else { return false };
    let Some(entry) = entries.get_mut(&webview_id) else { return false };
    if entry.phase != Phase::Creating || entry.browser_id.is_some() { return false; }
    entry.browser_id = Some(browser_id);
    entry.state = Some(state);
    entry.host = Some(host);
    if entry.close_requested { entry.phase = Phase::Closing { attached: false }; }
    true
  }

  pub(crate) fn attach(&self, webview_id: u32, browser_id: i32, browser: &crate::FrameNavigationState) -> bool {
    let Ok(mut entries) = self.0.lock() else { return false };
    let Some(entry) = entries.get_mut(&webview_id) else { return false };
    if !matches_browser(entry, browser_id, browser) || entry.phase != Phase::Creating { return false; }
    entry.phase = Phase::Attached;
    true
  }

  pub(crate) fn closing(&self, webview_id: u32, browser_id: i32, browser: &crate::FrameNavigationState) -> bool {
    let Ok(mut entries) = self.0.lock() else { return false };
    let Some(entry) = entries.get_mut(&webview_id) else { return false };
    if !matches_browser(entry, browser_id, browser) { return false; }
    match entry.phase {
      Phase::Creating => entry.phase = Phase::Closing { attached: false },
      Phase::Attached => entry.phase = Phase::Closing { attached: true },
      Phase::Closing { .. } | Phase::Destroying { .. } => return true,
      Phase::Reserved => return false,
      Phase::Closed { .. } => return false,
    }
    true
  }

  pub(crate) fn closed(&self, webview_id: u32, browser_id: i32, browser: &crate::FrameNavigationState) -> bool {
    let Ok(mut entries) = self.0.lock() else { return false };
    let Some(entry) = entries.get_mut(&webview_id) else { return false };
    if !matches_browser(entry, browser_id, browser) { return false; }
    let attached = match entry.phase {
      Phase::Creating => false,
      Phase::Attached => true,
      Phase::Closing { attached } | Phase::Destroying { attached } => attached,
      Phase::Closed { .. } => return false,
      Phase::Reserved => return false,
    };
    entry.phase = Phase::Closed { attached };
    let close = entry.close.clone();
    drop(entries);
    browser.invalidate_browser_lifetime();
    close.complete(crate::NativeBrowserCloseOutcome::Closed);
    true
  }

  pub(crate) fn acknowledge(&self, webview_id: u32, browser_id: i32, browser: &crate::FrameNavigationState) -> Option<bool> {
    let mut entries = self.0.lock().ok()?;
    let entry = entries.get(&webview_id)?;
    if !matches_browser(entry, browser_id, browser) { return None; }
    let Phase::Closed { attached } = entry.phase else { return None };
    entries.remove(&webview_id);
    Some(attached)
  }

  pub(crate) fn abort(&self, webview_id: u32) -> Option<BrowserHost> {
    let mut entries = self.0.lock().ok()?;
    let entry = entries.get_mut(&webview_id)?;
    if entry.browser_id.is_none() {
      let close = entry.close.clone();
      entries.remove(&webview_id);
      drop(entries);
      close.complete(crate::NativeBrowserCloseOutcome::NotCreated);
      return None;
    }
    let close = entry.close.clone();
    let host = if entry.phase == Phase::Creating {
      entry.phase = Phase::Closing { attached: false };
      entry.host.clone()
    } else {
      None
    };
    drop(entries);
    close.complete_creation(crate::NativeBrowserCreationOutcome::Failed);
    host
  }

  /// Called on the runtime UI thread for the reservation that originated the request.
  pub(crate) fn request_close(&self, webview_id: u32, close: &crate::NativeBrowserClose) -> Option<BrowserHost> {
    let mut entries = self.0.lock().ok()?;
    let entry = entries.get_mut(&webview_id)?;
    if !entry.close.same_reservation(close) { return None; }
    match entry.phase {
      Phase::Reserved => {
        let close = entry.close.clone();
        entries.remove(&webview_id);
        drop(entries);
        close.complete(crate::NativeBrowserCloseOutcome::NotCreated);
        None
      }
      Phase::Creating if entry.browser_id.is_none() => { entry.close_requested = true; None }
      Phase::Creating | Phase::Attached => {
        let attached = entry.phase == Phase::Attached;
        entry.phase = Phase::Closing { attached };
        entry.host.clone()
      }
      Phase::Closing { .. } | Phase::Destroying { .. } | Phase::Closed { .. } => None,
    }
  }

  pub(crate) fn closing_host(&self, webview_id: u32, browser_id: i32, browser: &crate::FrameNavigationState) -> Option<BrowserHost> {
    let mut entries = self.0.lock().ok()?;
    let entry = entries.get_mut(&webview_id)?;
    if !matches_browser(entry, browser_id, browser) { return None; }
    let Phase::Closing { attached } = entry.phase else { return None };
    entry.phase = Phase::Destroying { attached };
    entry.host.clone()
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.0.lock().is_ok_and(|entries| entries.is_empty())
  }

  pub(crate) fn contains(&self, webview_id: u32) -> bool {
    self.0.lock().map_or(true, |entries| entries.contains_key(&webview_id))
  }

  pub(crate) fn is_attached(&self, webview_id: u32, browser_id: i32, state: &crate::FrameNavigationState) -> bool {
    self.0.lock().is_ok_and(|entries| entries.get(&webview_id).is_some_and(|entry| {
      entry.browser_id == Some(browser_id) && entry.phase == Phase::Attached
        && entry.state.as_ref().is_some_and(|registered| registered.is_same_browser(state))
    }))
  }

  pub(crate) fn attached_host(&self, webview_id: u32, browser_id: i32, state: &crate::FrameNavigationState) -> Option<BrowserHost> {
    let entries = self.0.lock().ok()?;
    let entry = entries.get(&webview_id)?;
    if entry.browser_id != Some(browser_id) || entry.phase != Phase::Attached
      || !entry.state.as_ref().is_some_and(|registered| registered.is_same_browser(state)) {
      return None;
    }
    entry.host.clone()
  }

  pub(crate) fn has_unattached_for_window(&self, window_id: WindowId) -> bool {
    self.0.lock().map_or(true, |entries| entries.values().any(|entry| {
      entry.window_id == window_id && matches!(entry.phase,
        Phase::Creating | Phase::Closing { attached: false } | Phase::Destroying { attached: false } | Phase::Closed { attached: false })
    }))
  }
}

fn matches_browser(entry: &Entry, browser_id: i32, browser: &crate::FrameNavigationState) -> bool {
  entry.browser_id == Some(browser_id)
    && entry.state.as_ref().is_some_and(|registered| registered.is_same_browser(browser))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn created(lifetimes: &BrowserLifetimes, webview: u32, browser: i32) -> crate::FrameNavigationState {
    assert!(lifetimes.reserve(webview, 1.into()).is_some());
    assert!(lifetimes.begin(webview, 1.into()));
    let state = crate::FrameNavigationState::new();
    let mut entries = lifetimes.0.lock().unwrap();
    let entry = entries.get_mut(&webview).unwrap();
    entry.browser_id = Some(browser);
    entry.state = Some(state.clone());
    state
  }

  #[test]
  fn failed_creation_never_removes_another_browser() {
    let lifetimes = BrowserLifetimes::default();
    let first = created(&lifetimes, 1, 11);
    assert!(lifetimes.attach(1, 11, &first));
    let failed = created(&lifetimes, 2, 12);
    assert!(lifetimes.closing(2, 12, &failed));
    assert!(lifetimes.closed(2, 12, &failed));
    assert_eq!(lifetimes.acknowledge(2, 12, &failed), Some(false));
    assert!(!lifetimes.is_empty());
    assert!(lifetimes.closed(1, 11, &first));
    assert_eq!(lifetimes.acknowledge(1, 11, &first), Some(true));
    assert!(lifetimes.is_empty());
  }

  #[test]
  fn close_before_attach_and_duplicate_ack_are_rejected() {
    let lifetimes = BrowserLifetimes::default();
    let first = created(&lifetimes, 1, 11);
    assert!(lifetimes.closed(1, 11, &first));
    assert!(!lifetimes.attach(1, 11, &first));
    assert_eq!(lifetimes.acknowledge(1, 11, &first), Some(false));
    assert_eq!(lifetimes.acknowledge(1, 11, &first), None);
    let replacement = created(&lifetimes, 1, 11);
    assert!(lifetimes.attach(1, 11, &replacement));
    assert!(!lifetimes.closed(1, 11, &first));
    assert_eq!(lifetimes.acknowledge(1, 11, &first), None);
    assert!(lifetimes.closed(1, 11, &replacement));
    assert_eq!(lifetimes.acknowledge(1, 11, &replacement), Some(true));
  }

  #[test]
  fn matching_numeric_ids_do_not_replace_native_lifetime_identity() {
    let lifetimes = BrowserLifetimes::default();
    let original = created(&lifetimes, 1, 11);
    let replacement = crate::FrameNavigationState::new();
    assert!(lifetimes.attach(1, 11, &original));
    assert!(lifetimes.is_attached(1, 11, &original));
    assert!(!lifetimes.is_attached(1, 11, &replacement));
    assert!(lifetimes.closing(1, 11, &original));
    assert!(!lifetimes.is_attached(1, 11, &original));
  }

  #[test]
  fn queued_close_before_creation_cannot_attach_or_complete_a_different_reservation() {
    let lifetimes = BrowserLifetimes::default();
    let close = lifetimes.reserve(8, 1.into()).unwrap();
    assert!(lifetimes.request_close(8, &crate::NativeBrowserClose::default()).is_none());
    assert!(lifetimes.contains(8));
    assert!(lifetimes.request_close(8, &close).is_none());
    assert_eq!(close.outcomes(), (Some(crate::NativeBrowserCreationOutcome::Failed), Some(crate::NativeBrowserCloseOutcome::NotCreated)));
    assert!(!lifetimes.begin(8, 1.into()));
  }

  #[test]
  fn close_after_native_creation_before_attach_waits_for_close_ack() {
    let lifetimes = BrowserLifetimes::default();
    let state = created(&lifetimes, 11, 101);
    let close = lifetimes.close_handle(11).unwrap();
    assert!(lifetimes.request_close(11, &close).is_none());
    assert!(!lifetimes.attach(11, 101, &state));
    assert_eq!(close.outcomes().1, None);
    assert!(lifetimes.closed(11, 101, &state));
    assert_eq!(close.outcomes().1, Some(crate::NativeBrowserCloseOutcome::Closed));
  }

  #[test]
  fn failed_after_created_waits_for_native_close_acknowledgement() {
    let lifetimes = BrowserLifetimes::default();
    let state = created(&lifetimes, 9, 99);
    let close = lifetimes.close_handle(9).unwrap();
    assert!(lifetimes.abort(9).is_none());
    assert_eq!(close.outcomes(), (Some(crate::NativeBrowserCreationOutcome::Failed), None));
    assert!(lifetimes.contains(9));
    assert!(lifetimes.closed(9, 99, &state));
    assert_eq!(close.outcomes(), (Some(crate::NativeBrowserCreationOutcome::Failed), Some(crate::NativeBrowserCloseOutcome::Closed)));
    assert!(!lifetimes.closed(9, 99, &state));
  }

  #[test]
  fn attached_creation_then_close_preserves_both_receipts() {
    let lifetimes = BrowserLifetimes::default();
    let state = created(&lifetimes, 10, 100);
    let close = lifetimes.close_handle(10).unwrap();
    assert!(lifetimes.attach(10, 100, &state));
    close.complete_creation(crate::NativeBrowserCreationOutcome::Attached);
    assert!(lifetimes.closed(10, 100, &state));
    assert_eq!(close.outcomes(), (Some(crate::NativeBrowserCreationOutcome::Attached), Some(crate::NativeBrowserCloseOutcome::Closed)));
  }

  #[test]
  fn abort_without_a_native_browser_releases_the_reservation() {
    let lifetimes = BrowserLifetimes::default();
    assert!(lifetimes.reserve(4, 1.into()).is_some());
    assert!(lifetimes.begin(4, 1.into()));
    assert!(lifetimes.abort(4).is_none());
    assert!(lifetimes.is_empty());
  }
}
