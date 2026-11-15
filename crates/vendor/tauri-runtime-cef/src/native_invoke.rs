// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex};

use cef::*;
use crate::{NativeContextToken, cef_impl::native_eval::NativeEvalError};

#[derive(Clone)]
pub(crate) struct NativeContextEndpoints {
  pub(crate) channel: NativeContextChannel,
  pub(crate) evaluation: NativeContextEvaluation,
  pub(crate) document_url: url::Url,
  pub(crate) renderer_process: crate::NativeRendererProcess,
}

pub(crate) struct NativeTauriMessage {
  pub(crate) origin: NativeContextToken,
  pub(crate) document_url: url::Url,
  pub(crate) cmd: String,
  pub(crate) callback: u32,
  pub(crate) error: u32,
  pub(crate) raw: bool,
  pub(crate) invoke_key: String,
  pub(crate) headers_json: String,
  pub(crate) body: Vec<u8>,
}

pub(crate) struct NativeInvocationTarget {
  pub(crate) tauri: Arc<dyn Fn(NativeTauriMessage) + Send + Sync>,
  pub(crate) registered: Arc<dyn Fn(&NativeContextToken) -> bool + Send + Sync>,
  pub(crate) sync: Arc<dyn Fn(NativeSyncRequest) + Send + Sync>,
}

/// Captured at the actual native entry, not reconstructed from a webview label.
pub struct NativeSyncRequest {
  pub origin: NativeContextToken,
  pub channel: String,
  pub body: Vec<u8>,
  pub(crate) reply: Option<NativeContextSyncReply>,
}

impl NativeSyncRequest {
  /// `None` rejects the call. `Some(Vec::new())` is a successful empty Raw reply.
  pub fn respond(mut self, response: Option<Vec<u8>>) {
    let mut accepted = response.is_some() && self.origin.is_verified();
    let mut value = response.as_ref().filter(|bytes| !bytes.is_empty())
      .and_then(|bytes| binary_value_create(Some(bytes)));
    if response.as_ref().is_some_and(|bytes| !bytes.is_empty()) && value.is_none() {
      accepted = false;
    }
    if let Some(reply) = self.reply.take() {
      reply.complete(i32::from(accepted), value.as_mut());
    }
  }
}

impl Drop for NativeSyncRequest {
  fn drop(&mut self) {
    if let Some(reply) = self.reply.take() { reply.complete(0, None); }
  }
}

/// A native-only sink for one exact context's Tauri callbacks and channels.
#[derive(Clone)]
pub struct NativeReplySink {
  pub(crate) origin: NativeContextToken,
  pub(crate) channel: NativeContextChannel,
}

/// Prepared outside owner gates. It can only enqueue to its captured native
/// endpoint; the UI task must admit the original token before delivery.
#[derive(Clone)]
pub struct NativeChannelQueue { sink: NativeReplySink }

impl NativeChannelQueue {
  pub fn enqueue(&self, callback: u32, index: u64, raw: bool, body: Vec<u8>, end: bool) -> Result<(), String> {
    self.sink.enqueue(callback, raw, body, Some((index, end)))
  }
}

impl NativeReplySink {
  pub fn channel_queue(&self) -> Option<NativeChannelQueue> {
    self.origin.is_verified().then(|| NativeChannelQueue { sink: self.clone() })
  }

  pub fn send(&self, callback: u32, raw: bool, body: Vec<u8>) -> Result<(), String> {
    if !self.origin.is_verified() { return Err("Native context is no longer available.".into()); }
    self.enqueue(callback, raw, body, None)
  }

  pub fn send_channel(&self, callback: u32, index: u64, raw: bool, body: Vec<u8>, end: bool) -> Result<(), String> {
    if !self.origin.is_verified() { return Err("Native context is no longer available.".into()); }
    self.enqueue(callback, raw, body, Some((index, end)))
  }

  fn enqueue(&self, callback: u32, raw: bool, body: Vec<u8>, indexed: Option<(u64, bool)>) -> Result<(), String> {
    // Enqueue is not an effect. It may run under the caller's native gate;
    // the UI task below acquires the original authority before pipe delivery.
    if self.origin.is_revoked() { return Err("Native context is no longer available.".into()); }
    let mut task = NativeReplyTask::new(self.clone(), callback, raw, Mutex::new(Some(body)), indexed);
    if post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task)) != 1 {
      return Err("Native reply could not be scheduled.".into());
    }
    Ok(())
  }
}

wrap_task! {
  struct NativeReplyTask {
    sink: NativeReplySink,
    callback: u32,
    raw: bool,
    body: Mutex<Option<Vec<u8>>>,
    indexed: Option<(u64, bool)>,
  }
  impl Task {
    fn execute(&self) {
      let Some(body) = self.body.lock().ok().and_then(|mut body| body.take()) else { return };
      let mut value = (!body.is_empty()).then(|| binary_value_create(Some(body.as_slice()))).flatten();
      if !body.is_empty() && value.is_none() { return; }
      // This CEF operation only enqueues a native pipe message. It runs no JS,
      // performs no modal/UI wait and is serialized with exact native revocation.
      let _ = self.sink.origin.with_live(|| match self.indexed {
        Some((index, end)) => self.sink.channel.reply_channel(self.callback, index,
          i32::from(self.raw), value.as_mut(), i32::from(end)),
        None => self.sink.channel.reply(self.callback, i32::from(self.raw), value.as_mut()),
      });
    }
  }
}
pub(crate) fn binary_bytes(value: Option<&BinaryValue>) -> Option<Vec<u8>> {
  let Some(value) = value else { return Some(Vec::new()) };
  let mut bytes = vec![0; value.size()];
  if !bytes.is_empty() && value.data(Some(&mut bytes), 0) != bytes.len() { return None; }
  Some(bytes)
}

pub(crate) fn eval_error(code: &str, payload: String) -> NativeEvalError {
  match code {
    "stale_context" => NativeEvalError::StaleContext,
    "cancelled" => NativeEvalError::Cancelled,
    "duplicate_request" => NativeEvalError::DuplicateRequest,
    "javascript" => NativeEvalError::JavaScript(payload),
    "serialization" => NativeEvalError::Serialization(payload),
    "transport_closed" => NativeEvalError::TransportClosed,
    _ => NativeEvalError::Internal,
  }
}
