// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex, Weak};
use cef::*;

pub(crate) type Completion = Box<dyn FnOnce(Result<Vec<u8>, String>) + Send>;
struct State {
  completion: Option<Completion>,
  registration: Option<cef::Registration>,
  close: Option<crate::NativeBrowserCloseSubscription>,
}
struct Request {
  browser: crate::FrameNavigationState,
  browser_id: i32,
  id: i32,
  state: Mutex<State>,
}
/// Retain this handle until completion; cancellation is idempotent.
#[derive(Clone)]
pub struct NativeBrowserProtocol { request: Arc<Request> }
impl NativeBrowserProtocol {
  pub fn cancel(&self) { self.request.finish(Err("Browser protocol request was cancelled.".into())); }
}
impl Request {
  fn take(&self) -> (Option<Completion>, Option<cef::Registration>, Option<crate::NativeBrowserCloseSubscription>) {
    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
    (state.completion.take(), state.registration.take(), state.close.take())
  }
  fn finish(&self, result: Result<Vec<u8>, String>) {
    let (completion, registration, close) = self.take();
    drop(registration);
    drop(close);
    if let Some(completion) = completion { crate::native_evaluation::defer_completion(Box::new(move || completion(result))); }
  }
  fn result(&self, success: bool, bytes: &[u8]) {
    // The immutable response is claimed inside the exact creation gate. No
    // CEF call, user callback, or event-loop wait runs under that gate.
    let accepted = self.browser.with_live_browser(|| self.take());
    let Some((completion, registration, close)) = accepted else {
      self.finish(Err("Original native browser closed before the response.".into()));
      return;
    };
    drop(registration);
    drop(close);
    if let Some(completion) = completion {
      let result = if success { Ok(bytes.to_vec()) }
        else { Err(format!("Browser protocol failed: {}", String::from_utf8_lossy(bytes))) };
      crate::native_evaluation::defer_completion(Box::new(move || completion(result)));
    }
  }
}
impl Drop for Request {
  fn drop(&mut self) {
    let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
    if let Some(completion) = state.completion.take() {
      crate::native_evaluation::defer_completion(Box::new(move || completion(Err("Browser protocol receipt was dropped.".into()))));
    }
  }
}

pub(crate) struct Dispatch {
  expected: crate::FrameNavigationState,
  message: Vec<u8>,
  request: Arc<Request>,
  started: bool,
}
impl Dispatch {
  pub(crate) fn new(expected: crate::FrameNavigationState, method: String, params: serde_json::Value,
    completion: Completion) -> Result<(Self, NativeBrowserProtocol), String> {
    if !matches!(method.as_str(), "Page.enable" | "Network.enable" | "Emulation.setDeviceMetricsOverride" | "Page.addScriptToEvaluateOnNewDocument") {
      return Err("Method is not a browser-scoped pre-navigation setup operation.".into());
    }
    let id = crate::devtools::allocate_runtime_devtools_message_id().map_err(|e| e.to_string())?;
    let message = serde_json::to_vec(&serde_json::json!({"id": id, "method": method, "params": params})).map_err(|e| e.to_string())?;
    let browser_id = expected.bound_browser_id().ok_or("Original native browser is unavailable.")?;
    let request = Arc::new(Request { browser: expected.clone(), browser_id, id,
      state: Mutex::new(State { completion: Some(completion), registration: None, close: None }) });
    Ok((Self { expected, message, request: request.clone(), started: false }, NativeBrowserProtocol { request }))
  }
  pub(crate) fn execute(mut self, browser: &Browser, state: &crate::FrameNavigationState) {
    if !state.is_same_browser(&self.expected) || !state.has_browser_id(browser.identifier())
      || browser.identifier() != self.request.browser_id {
      self.request.finish(Err("Original native browser was replaced or closed.".into())); return;
    }
    let weak = Arc::downgrade(&self.request);
    let close = match state.when_closed(move |_| {
      if let Some(request) = weak.upgrade() { request.finish(Err("Original native browser closed.".into())); }
    }) {
      Ok(close) => close,
      Err(error) => { self.request.finish(Err(error)); return; }
    };
    let Some(host) = browser.host() else { self.request.finish(Err("Original native browser host is unavailable.".into())); return };
    let mut observer = BrowserProtocolObserver::new(Arc::downgrade(&self.request));
    let Some(registration) = host.add_dev_tools_message_observer(Some(&mut observer)) else {
      self.request.finish(Err("Browser protocol observer could not be installed.".into())); return;
    };
    {
      let mut pending = self.request.state.lock().unwrap_or_else(|e| e.into_inner());
      if pending.completion.is_none() { return; }
      pending.registration = Some(registration);
      pending.close = Some(close);
    }
    if !state.has_browser_id(browser.identifier()) {
      self.request.finish(Err("Original native browser closed before dispatch.".into())); return;
    }
    self.started = true;
    if host.send_dev_tools_message(Some(&self.message)) != 1 {
      self.request.finish(Err("Browser protocol dispatch failed.".into()));
    }
  }
}
impl Drop for Dispatch {
  fn drop(&mut self) { if !self.started { self.request.finish(Err("Browser protocol was not dispatched.".into())); } }
}
wrap_dev_tools_message_observer! {
  struct BrowserProtocolObserver { request: Weak<Request> }
  impl DevToolsMessageObserver {
    fn on_dev_tools_method_result(&self, browser: Option<&mut Browser>, id: i32, success: i32, result: Option<&[u8]>) {
      let Some(request) = self.request.upgrade() else { return };
      if id != request.id || browser.is_none_or(|browser| browser.identifier() != request.browser_id) { return; }
      request.result(success != 0, result.unwrap_or_default());
    }
    fn on_dev_tools_agent_detached(&self, _browser: Option<&mut Browser>) {
      if let Some(request) = self.request.upgrade() { request.finish(Err("Original browser DevTools agent detached.".into())); }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::mpsc;
  use std::time::Duration;

  fn request(browser: crate::FrameNavigationState, tx: mpsc::Sender<Result<Vec<u8>, String>>) -> Request {
    Request { browser, browser_id: 7, id: 42, state: Mutex::new(State {
      completion: Some(Box::new(move |result| { let _ = tx.send(result); })),
      registration: None, close: None,
    }) }
  }

  #[test]
  fn accepts_once_and_does_not_deliver_after_close() {
    let browser = crate::FrameNavigationState::new();
    assert!(browser.bind_browser(7));
    let (tx, rx) = mpsc::channel();
    let original = request(browser.clone(), tx);
    original.result(true, b"first");
    original.result(true, b"duplicate");
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(), b"first");
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());

    let (tx, rx) = mpsc::channel();
    let stale = request(browser.clone(), tx);
    browser.invalidate_browser_lifetime();
    let replacement = crate::FrameNavigationState::new();
    assert!(replacement.bind_browser(7));
    stale.result(true, b"stale");
    assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
    assert!(replacement.with_live_browser(|| ()).is_some());
  }
}
