// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::Mutex;

type Pending = Box<dyn FnOnce() + Send>;
#[derive(Default)]
struct State {
  root: Option<String>,
  enabled: bool,
  armed: bool,
  ready: bool,
  failed: bool,
  waiting: Vec<Pending>,
}
#[derive(Default)]
pub(crate) struct NavigationGate(Mutex<State>);
impl NavigationGate {
  pub(crate) fn enabled(&self) -> bool {
    let Ok(mut state) = self.0.lock() else { return false };
    if state.failed { return false; }
    state.enabled = true;
    state.root.is_some()
  }
  pub(crate) fn tree(&self, result: &[u8]) -> Result<bool, ()> {
    let value = serde_json::from_slice::<serde_json::Value>(result).map_err(|_| ())?;
    let root = value.pointer("/frameTree/frame/id").and_then(|v| v.as_str()).filter(|v| !v.is_empty()).ok_or(())?;
    let mut state = self.0.lock().map_err(|_| ())?;
    if state.failed { return Err(()); }
    if state.root.is_none() { state.root = Some(root.to_owned()); }
    Ok(state.enabled)
  }
  pub(crate) fn fail(&self) {
    let waiting = if let Ok(mut state) = self.0.lock() { state.failed = true; std::mem::take(&mut state.waiting) } else { Vec::new() };
    drop(waiting);
  }
  pub(crate) fn arm(&self) { if let Ok(mut state) = self.0.lock() { state.armed = true; } }
  pub(crate) fn pending(&self) -> bool { self.0.lock().map_or(true, |state| !state.ready) }
  pub(crate) fn submit(&self, task: Pending) {
    let mut task = Some(task);
    if let Ok(mut state) = self.0.lock() {
      if state.failed { return; }
      if !state.ready { state.waiting.push(task.take().expect("pending task")); }
    } else { return; }
    if let Some(task) = task { task(); }
  }
  pub(crate) fn release(&self) -> bool {
    let tasks = {
      let Ok(mut state) = self.0.lock() else { return false };
      if state.failed || !state.enabled || state.root.is_none() || state.ready { return false; }
      state.ready = true;
      std::mem::take(&mut state.waiting)
    };
    for task in tasks { task(); }
    true
  }
  pub(crate) fn event(&self, method: &str, params: &[u8]) -> Option<crate::NavigationStart> {
    let value = serde_json::from_slice::<serde_json::Value>(params).ok()?;
    if method == "Page.frameNavigated" {
      let frame = value.get("frame")?;
      if frame.get("parentId").is_none() {
        let root = frame.get("id")?.as_str()?.to_owned();
        self.0.lock().ok()?.root = Some(root);
      }
      return None;
    }
    if method != "Page.frameStartedNavigating" { return None; }
    let id = value.get("frameId")?.as_str()?;
    let state = self.0.lock().ok()?;
    if !state.armed || state.root.as_deref() != Some(id) { return None; }
    let same_document = match value.get("navigationType")?.as_str()? {
      "sameDocument" | "historySameDocument" => true,
      "reload" | "reloadBypassingCache" | "restore" | "restoreWithPost" | "historyDifferentDocument" | "differentDocument" => false,
      _ => return None,
    };
    Some(crate::NavigationStart { url: value.get("url")?.as_str()?.to_owned(), same_document })
  }
}
/// Chromium's main-frame navigation start, before commit or failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavigationStart { pub url: String, pub same_document: bool }

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;
  #[test]
  fn gate_waits_for_both_acknowledgments_and_drains_in_order() {
    let gate = NavigationGate::default();
    let observed = Arc::new(Mutex::new(Vec::new()));
    for value in [1,2] { let observed = observed.clone(); gate.submit(Box::new(move || observed.lock().unwrap().push(value))); }
    assert!(!gate.enabled());
    assert!(!gate.release());
    assert_eq!(gate.tree(br#"{"frameTree":{"frame":{"id":"root"}}}"#), Ok(true));
    assert!(gate.release());
    assert_eq!(*observed.lock().unwrap(), vec![1,2]);
  }
  #[test]
  fn failure_discards_pending_without_running_them() {
    let gate = NavigationGate::default();
    let observed = Arc::new(Mutex::new(false));
    let value = observed.clone();
    gate.submit(Box::new(move || *value.lock().unwrap() = true));
    gate.fail();
    assert!(!gate.release());
    assert!(!*observed.lock().unwrap());
    assert!(gate.tree(br#"{"frameTree":{"frame":{"id":"root"}}}"#).is_err());
  }
  #[test]
  fn navigation_type_comes_from_protocol_not_url() {
    let gate = NavigationGate::default();
    gate.tree(br#"{"frameTree":{"frame":{"id":"root"}}}"#).unwrap();
    gate.arm();
    let event = |kind| format!(r#"{{"frameId":"root","url":"https://example.test/#same","navigationType":"{kind}"}}"#);
    assert!(gate.event("Page.frameStartedNavigating",event("sameDocument").as_bytes()).unwrap().same_document);
    assert!(!gate.event("Page.frameStartedNavigating",event("differentDocument").as_bytes()).unwrap().same_document);
    assert!(gate.event("Page.frameStartedNavigating",br#"{"frameId":"child","url":"x","navigationType":"reload"}"#).is_none());
  }
}
