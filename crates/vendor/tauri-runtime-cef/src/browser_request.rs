// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex};
use cef::{Browser, Frame, ImplBrowser, ImplFrame, CefString};
use crate::FrameNavigationState;

pub type BrowserResourceRequestHandler = dyn Fn(&BrowserResourceRequest) -> bool + Send + Sync;

/// Stock CEF network-request observation, not V8/document invocation proof.
/// The policy belongs to one native browser creation and is not inherited by popups.
pub struct BrowserResourceRequest {
  browser: FrameNavigationState,
  browser_id: Option<i32>,
  frame_id: Option<String>,
  main_frame: Option<bool>,
  url: String,
  protocol: Option<String>,
}
impl BrowserResourceRequest {
  pub fn browser_state(&self) -> &FrameNavigationState { &self.browser }
  pub fn browser_id(&self) -> Option<i32> { self.browser_id }
  pub fn frame_id(&self) -> Option<&str> { self.frame_id.as_deref() }
  pub fn frame_is_main(&self) -> Option<bool> { self.main_frame }
  pub fn url(&self) -> &str { &self.url }
  /// An exact factory registration accepted for this browser's request context.
  /// This is not inferred from an arbitrary `.localhost` suffix.
  pub fn bound_protocol(&self) -> Option<&str> { self.protocol.as_deref() }
}

struct Binding { name: String, scheme: String, host: Option<String> }
#[derive(Clone, Default)]
pub(crate) struct RegisteredProtocols(Arc<Mutex<Vec<Binding>>>);
impl RegisteredProtocols {
  pub(crate) fn alias(&self, name: &str, scheme: &str) {
    self.0.lock().unwrap_or_else(|error| error.into_inner()).push(Binding {
      name: name.to_owned(), scheme: scheme.to_owned(), host: Some(format!("{name}.localhost")),
    });
  }
  pub(crate) fn native(&self, name: &str) {
    self.0.lock().unwrap_or_else(|error| error.into_inner()).push(Binding {
      name: name.to_owned(), scheme: name.to_owned(), host: None,
    });
  }
  pub(crate) fn resolve(&self, value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    self.0.lock().ok()?.iter().find(|binding|
      binding.scheme == url.scheme() && binding.host.as_deref().is_none_or(|host| url.host_str() == Some(host)))
      .map(|binding| binding.name.clone())
  }
}

pub(crate) struct BrowserRequestPolicy {
  pub(crate) browser: FrameNavigationState,
  pub(crate) protocols: RegisteredProtocols,
  pub(crate) handler: Arc<BrowserResourceRequestHandler>,
}
impl BrowserRequestPolicy {
  pub(crate) fn decide(&self, browser: Option<&Browser>, frame: Option<&Frame>, url: String) -> bool {
    let browser_id = browser.map(|browser| browser.identifier());
    let owned = browser_id.is_some_and(|id| self.browser.has_browser_id(id));
    if !owned { return true; }
    let request = BrowserResourceRequest {
      browser: self.browser.clone(), browser_id,
      frame_id: frame.map(|frame| CefString::from(&frame.identifier()).to_string()),
      main_frame: frame.map(|frame| frame.is_main() != 0),
      protocol: owned.then(|| self.protocols.resolve(&url)).flatten(), url,
    };
    let allow = (self.handler)(&request);
    allow
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn only_accepted_exact_factory_hosts_are_classified_owned() {
    let protocols = RegisteredProtocols::default();
    assert!(protocols.resolve("http://genoffice-app-diagram.localhost/index.html").is_none());
    protocols.alias("genoffice-app-diagram", "http");
    assert_eq!(protocols.resolve("http://genoffice-app-diagram.localhost/index.html").as_deref(), Some("genoffice-app-diagram"));
    assert!(protocols.resolve("http://genoffice-app-diagram.localhost.evil.test/index.html").is_none());
    assert!(protocols.resolve("https://genoffice-app-diagram.localhost/index.html").is_none());
    protocols.native("drawio-app");
    assert_eq!(protocols.resolve("drawio-app://editor/index.html").as_deref(), Some("drawio-app"));
  }
}
