// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{collections::HashMap, path::{Path, PathBuf}, sync::{Arc, LazyLock, Mutex, Weak}};
use cef::*;
use crate::browser_request::RegisteredProtocols;

pub type NetworkSessionRequestHandler = dyn Fn(&NetworkSessionRequest<'_>) -> bool + Send + Sync;

/// A request from the actual dedicated CEF request context, including workers.
/// Absence of a browser/frame stays absent and never selects a focused view.
pub struct NetworkSessionRequest<'a> {
  session: &'a NativeNetworkSession,
  browser_id: Option<i32>,
  frame_id: Option<String>,
  main_frame: Option<bool>,
  url: &'a str,
  protocol: Option<String>,
}
impl NetworkSessionRequest<'_> {
  pub fn session(&self) -> &NativeNetworkSession { self.session }
  pub fn partition(&self) -> &str { self.session.partition() }
  pub fn browser_id(&self) -> Option<i32> { self.browser_id }
  pub fn frame_id(&self) -> Option<&str> { self.frame_id.as_deref() }
  pub fn frame_is_main(&self) -> Option<bool> { self.main_frame }
  pub fn url(&self) -> &str { self.url }
  pub fn bound_protocol(&self) -> Option<&str> { self.protocol.as_deref() }
}

struct State {
  name: String,
  partition: String,
  handler: Arc<NetworkSessionRequestHandler>,
  protocols: RegisteredProtocols,
  context: Mutex<Option<RequestContext>>,
}

/// Share one clone across every browser in the original persistent partition.
/// It retains its native request context independently of any root browser.
#[derive(Clone)]
pub struct NativeNetworkSession(Arc<State>);
#[derive(Clone)]
pub(crate) struct NetworkSessionPolicy(Weak<State>);

static SESSIONS: LazyLock<Mutex<HashMap<PathBuf, Weak<State>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

impl std::fmt::Debug for NativeNetworkSession {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("NativeNetworkSession").finish_non_exhaustive() }
}

impl NativeNetworkSession {
  pub fn persistent(name: &str, handler: impl Fn(&NetworkSessionRequest<'_>) -> bool + Send + Sync + 'static) -> Result<Self, String> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')) {
      return Err("Native persistent network partition name is invalid.".into());
    }
    Ok(Self(Arc::new(State { name: name.to_owned(), partition: format!("persist:{name}"), handler: Arc::new(handler),
      protocols: RegisteredProtocols::default(), context: Mutex::new(None) })))
  }
  pub fn partition(&self) -> &str { &self.0.partition }
  pub fn same_session(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }
  pub(crate) fn policy(&self) -> NetworkSessionPolicy { NetworkSessionPolicy(Arc::downgrade(&self.0)) }
  pub(crate) fn protocols(&self) -> RegisteredProtocols { self.0.protocols.clone() }

  pub(crate) fn cache_path(&self, root: &Path) -> Result<PathBuf, String> {
    let path = root.join("NetworkSessions").join(&self.0.name);
    let mut sessions = SESSIONS.lock().map_err(|_| "Native network session registry was poisoned.")?;
    sessions.retain(|_, session| session.strong_count() != 0);
    if sessions.get(&path).and_then(Weak::upgrade).is_some_and(|session| !Arc::ptr_eq(&session, &self.0)) {
      return Err("Persistent network partition must reuse its original native session object.".into());
    }
    sessions.insert(path.clone(), Arc::downgrade(&self.0));
    Ok(path)
  }

  pub(crate) fn retain_context(&self, context: &RequestContext) {
    let mut retained = self.0.context.lock().unwrap_or_else(|error| error.into_inner());
    if retained.is_none() { *retained = Some(context.clone()); }
  }
}

impl NetworkSessionPolicy {
  pub(crate) fn decide(&self, browser: Option<&Browser>, frame: Option<&Frame>, url: &str) -> bool {
    let Some(state) = self.0.upgrade() else { return false };
    let session = NativeNetworkSession(state);
    let request = NetworkSessionRequest {
      session: &session, browser_id: browser.map(|browser| browser.identifier()),
      frame_id: frame.map(|frame| CefString::from(&frame.identifier()).to_string()),
      main_frame: frame.map(|frame| frame.is_main() != 0),
      protocol: session.0.protocols.resolve(url), url,
    };
    (session.0.handler)(&request)
  }
}

wrap_resource_request_handler! {
  pub(crate) struct SessionResourceHandler { policy: NetworkSessionPolicy }
  impl ResourceRequestHandler {
    fn on_before_resource_load(
      &self, browser: Option<&mut Browser>, frame: Option<&mut Frame>,
      request: Option<&mut Request>, _callback: Option<&mut Callback>,
    ) -> ReturnValue {
      let Some(request) = request else { return sys::cef_return_value_t::RV_CANCEL.into() };
      let url = CefString::from(&request.url()).to_string();
      if self.policy.decide(browser.as_deref(), frame.as_deref(), &url) {
        sys::cef_return_value_t::RV_CONTINUE.into()
      } else { sys::cef_return_value_t::RV_CANCEL.into() }
    }
  }
}

pub(crate) fn release_request_contexts() {
  let sessions: Vec<_> = SESSIONS.lock().unwrap_or_else(|error| error.into_inner()).values().filter_map(Weak::upgrade).collect();
  for session in sessions {
    let context = session.context.lock().unwrap_or_else(|error| error.into_inner()).take();
    drop(context);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn partition_configuration_is_explicit_and_clones_keep_the_same_owner() {
    let session = NativeNetworkSession::persistent("ieum-diagram", |_| false).unwrap();
    assert_eq!(session.partition(), "persist:ieum-diagram");
    assert!(session.same_session(&session.clone()));
    assert!(!session.same_session(&NativeNetworkSession::persistent("other", |_| true).unwrap()));
    assert!(NativeNetworkSession::persistent("../outside", |_| true).is_err());
  }
}
