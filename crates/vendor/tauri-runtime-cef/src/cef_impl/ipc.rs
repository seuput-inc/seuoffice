// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex};

use cef::*;
use tauri_runtime::{UserEvent, webview::DetachedWebview};

use crate::{
  cef_impl::client::TauriCefBrowserClient, runtime::CefRuntime, webview::CefWebviewDispatcher,
};

const IPC_MESSAGE_NAME: &str = "tauri:ipc";
const RENDERER_PID_MESSAGE: &str = "tauri:renderer-pid";

pub(crate) struct RendererBrowser {
  initialization: crate::cef_impl::trusted_init::Initialization,
  active: bool,
}

fn send_context_message(frame: &Frame, name: &str, seed: &str, capability: &str) -> bool {
  let Some(mut message) = process_message_create(Some(&CefString::from(name))) else { return false };
  let Some(args) = message.argument_list() else { return false };
  if args.set_string(0, Some(&CefString::from(seed))) == 0
    || args.set_string(1, Some(&CefString::from(capability))) == 0 { return false; }
  frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
  true
}
const IPC_POST_MESSAGE_FUNCTION: &str = "postMessage";

pub(crate) type IpcHandler<T> =
  dyn Fn(DetachedWebview<T, CefRuntime<T>>, http::Request<String>) + Send;

wrap_v8_handler! {
  struct IpcPostMessageV8Handler;

  impl V8Handler {
    fn execute(
      &self,
      name: Option<&CefString>,
      _object: Option<&mut V8Value>,
      arguments: Option<&[Option<V8Value>]>,
      retval: Option<&mut Option<V8Value>>,
      exception: Option<&mut CefString>,
    ) -> std::os::raw::c_int {
      let Some(name) = name else {
        return 0;
      };
      if name.to_string() != IPC_POST_MESSAGE_FUNCTION {
        return 0;
      }

      let Some(message) = arguments
        .filter(|arguments| arguments.len() == 1)
        .and_then(|arguments| arguments[0].as_ref())
        .filter(|argument| argument.is_string() != 0)
      else {
        if let Some(exception) = exception {
          *exception = CefString::from("window.ipc.postMessage expects a string argument");
        }
        return 1;
      };

      let Some(context) = v8_context_get_current_context() else {
        return 1;
      };
      let Some(frame) = context.frame() else {
        return 1;
      };

      let body = CefString::from(&message.string_value()).to_string();
      let url = CefString::from(&frame.url()).to_string();
      let mut process_message = process_message_create(Some(&CefString::from(IPC_MESSAGE_NAME)));
      if let Some(args) = process_message
        .as_ref()
        .and_then(ProcessMessage::argument_list)
      {
        args.set_string(0, Some(&CefString::from(url.as_str())));
        args.set_string(1, Some(&CefString::from(body.as_str())));
        frame.send_process_message(ProcessId::BROWSER, process_message.as_mut());
      }

      if let Some(retval) = retval {
        *retval = v8_value_create_undefined();
      }
      1
    }
  }
}

fn install_ipc_post_message(context: Option<&mut V8Context>) {
  let Some(window) = context.and_then(|context| context.global()) else {
    return;
  };
  let attributes = sys::cef_v8_propertyattribute_t(
    [
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY,
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTENUM,
      sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTDELETE,
    ]
    .into_iter()
    .fold(0, |acc, attr| acc | attr.0),
  )
  .into();
  let Some(mut ipc) = v8_value_create_object(None, None) else {
    return;
  };
  let mut handler = IpcPostMessageV8Handler::new();
  let post_message_name = CefString::from(IPC_POST_MESSAGE_FUNCTION);
  let Some(mut post_message) =
    v8_value_create_function(Some(&post_message_name), Some(&mut handler))
  else {
    return;
  };
  ipc.set_value_bykey(
    Some(&post_message_name),
    Some(&mut post_message),
    attributes,
  );
  window.set_value_bykey(Some(&CefString::from("ipc")), Some(&mut ipc), attributes);
}

wrap_render_process_handler! {
  pub struct TauriRenderProcessHandler {
    browsers: Arc<Mutex<std::collections::HashMap<i32, RendererBrowser>>>,
  }

  impl RenderProcessHandler {
    fn on_browser_created(&self, browser: Option<&mut Browser>, extra_info: Option<&mut DictionaryValue>) {
      let (Some(browser), Some(extra_info)) = (browser, extra_info) else { return };
      let payload = CefString::from(&extra_info.string(Some(&CefString::from("seuoffice.trusted-init")))).to_string();
      let Ok(initialization) = serde_json::from_str::<crate::cef_impl::trusted_init::Initialization>(&payload) else { return };
      if let Ok(mut browsers) = self.browsers.lock() {
        browsers.insert(browser.identifier(), RendererBrowser { initialization, active: false });
      }
    }

    fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
      if let (Some(browser), Ok(mut browsers)) = (browser, self.browsers.lock()) {
        browsers.remove(&browser.identifier());
      }
    }

    fn on_process_message_received(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      source_process: ProcessId,
      message: Option<&mut ProcessMessage>,
    ) -> std::os::raw::c_int {
      if source_process != ProcessId::BROWSER { return 0; }
      let (Some(browser), Some(frame), Some(message)) = (browser, frame, message) else { return 0 };
      if frame.is_main() == 0 || CefString::from(&message.name()).to_string() != crate::cef_impl::trusted_activation::activation_message_name() { return 0; }
      let Some(args) = message.argument_list() else { return 1 };
      let seed = CefString::from(&args.string(0)).to_string();
      let Ok(mut browsers) = self.browsers.lock() else { return 1 };
      let Some(entry) = browsers.get_mut(&browser.identifier()) else { return 1 };
      if entry.active || entry.initialization.seed != seed { return 1; }
      entry.active = true;
      drop(browsers);
      send_context_message(frame, crate::cef_impl::trusted_activation::ACK, &seed, "");
      1
    }

    fn on_context_created(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      context: Option<&mut V8Context>,
    ) {
      let (Some(browser), Some(frame), Some(context)) = (browser, frame, context) else { return };
      let protected = self.browsers.lock().is_ok_and(|browsers| browsers.contains_key(&browser.identifier()));
      if !protected { install_ipc_post_message(Some(&mut *context)); }
      if frame.is_main() != 0 {
        let mut message = process_message_create(Some(&CefString::from(RENDERER_PID_MESSAGE)));
        if let Some(args) = message.as_ref().and_then(ProcessMessage::argument_list) {
          args.set_string(0, Some(&CefString::from(std::process::id().to_string().as_str())));
          frame.send_process_message(ProcessId::BROWSER, message.as_mut());
        }
      }
    }

  }
}

pub(crate) fn record_renderer_identity(
  browser: Option<&Browser>, frame: Option<&Frame>, source: ProcessId, message: Option<&ProcessMessage>,
) {
  if source != ProcessId::RENDERER { return; }
  let (Some(browser), Some(frame), Some(message)) = (browser, frame, message) else { return; };
  if frame.is_main() == 0 || CefString::from(&message.name()).to_string() != RENDERER_PID_MESSAGE { return; }
  let Some(args) = message.argument_list() else { return; };
  if let Ok(pid) = CefString::from(&args.string(0)).to_string().parse::<u32>() {
    crate::renderer_process::identified(browser.identifier(), pid);
  }
}

/// Consumer for the private CEF registration callback only. `intent` must be
/// supplied by the browser-owned approved navigation, not a process message
/// or JavaScript. The native callback must already have checked the RFH,
/// pending/committed document and one-shot native channel.
pub(crate) fn on_native_context_registered<T: UserEvent>(
  client: &TauriCefBrowserClient<T>,
  browser: &mut Browser,
  frame: &mut Frame,
  frame_id: &str,
  identity: crate::native_context::VerifiedNativeIdentity,
  issuance: u64,
  intent: &str,
  endpoints: crate::native_invoke::NativeContextEndpoints,
) -> bool {
  if frame.is_main() == 0 || CefString::from(&frame.identifier()).to_string() != frame_id { return false; }
  let browser_id = browser.identifier();
  let state = &client.frame_navigation_state;
  let Some(target) = state.native_contexts.invocation_target(state, browser_id) else { return false };
  let Some(origin) = state.native_contexts.register_verified(
    state.clone(), browser_id, frame_id.to_owned(), identity.clone(), issuance, intent,
  ) else { return false };
  if !state.native_contexts.attach_endpoints(&origin, endpoints) {
    let _ = state.native_contexts.revoke_verified(state, browser_id, frame_id, &identity);
    return false;
  }
  state.native_reload.registered(&origin);
  if !(target.registered)(&origin) {
    state.native_reload.closed();
    let _ = state.native_contexts.revoke_verified(state, browser_id, frame_id, &identity);
    return false;
  }
  if !state.native_contexts.observe_process_exit(&origin, client.handlers.frame_event_handler.clone()) {
    state.native_reload.closed();
    let _ = state.native_contexts.revoke_verified(state, browser_id, frame_id, &identity);
    return false;
  }
  if let Some(handler) = &client.handlers.frame_event_handler {
    let mut task = EmitNativeRegistration::new(handler.clone(), origin.clone(), browser_id, frame_id.to_owned());
    if post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task)) != 1 {
      state.native_reload.closed();
      let _ = state.native_contexts.revoke_verified(state, browser_id, frame_id, &identity);
      return false;
    }
  }
  origin.is_verified()
}

wrap_task! {
  struct EmitNativeRegistration {
    handler: Arc<crate::FrameEventHandler>,
    origin: crate::NativeContextToken,
    browser_id: i32,
    frame_id: String,
  }
  impl Task {
    fn execute(&self) {
      if !self.origin.is_verified() { return; }
      (self.handler)(crate::FrameEvent { browser_id: self.browser_id,
        frame_id: self.frame_id.clone(), is_main: true,
        kind: crate::FrameEventKind::NativeContextRegistered { origin: self.origin.clone() } });
    }
  }
}

pub(crate) fn on_native_tauri_request<T: UserEvent>(
  client: &TauriCefBrowserClient<T>, browser_id: i32, frame_id: &str,
  identity: &crate::native_context::VerifiedNativeIdentity,
  cmd: String, callback: u32, error: u32, raw: bool, invoke_key: String, headers_json: String, body: Vec<u8>,
) {
  let state = &client.frame_navigation_state;
  let Some(origin) = state.native_contexts.verified_token(state, browser_id, frame_id, identity) else { return };
  let Some(endpoints) = origin.endpoints() else { return };
  let Some(target) = state.native_contexts.invocation_target(state, browser_id) else { return };
  (target.tauri)(crate::native_invoke::NativeTauriMessage {
    origin, document_url: endpoints.document_url, cmd, callback, error, raw, invoke_key, headers_json, body,
  });
}

pub(crate) fn on_native_sync_request<T: UserEvent>(
  client: &TauriCefBrowserClient<T>, browser_id: i32, frame_id: &str,
  identity: &crate::native_context::VerifiedNativeIdentity,
  channel: String, body: Vec<u8>, reply: NativeContextSyncReply,
) {
  let state = &client.frame_navigation_state;
  let Some(origin) = state.native_contexts.verified_token(state, browser_id, frame_id, identity) else {
    reply.complete(0, None);
    return;
  };
  let Some(target) = state.native_contexts.invocation_target(state, browser_id) else {
    reply.complete(0, None);
    return;
  };
  (target.sync)(crate::NativeSyncRequest { origin, channel, body, reply: Some(reply) });
}

pub(crate) fn on_native_first_nonempty_layout<T: UserEvent>(
  client: &TauriCefBrowserClient<T>, browser_id: i32, frame_id: &str,
  identity: &crate::native_context::VerifiedNativeIdentity,
) {
  let state = &client.frame_navigation_state;
  let Some(origin) = state.native_contexts.verified_token(state, browser_id, frame_id, identity) else { return };
  if let Some(handler) = &client.handlers.frame_event_handler {
    handler(crate::FrameEvent { browser_id, frame_id: frame_id.to_owned(), is_main: true,
      kind: crate::FrameEventKind::FirstVisuallyNonEmptyLayout { origin } });
  }
}

/// Claim the exact RFH process loss before ordinary frame pre-revocation.
/// Nothing outside the short registry mutation is executed in the CEF hook.
pub(crate) fn on_native_context_process_will_exit<T: UserEvent>(
  client: &TauriCefBrowserClient<T>, browser_id: i32, frame_id: &str,
  identity: &crate::native_context::VerifiedNativeIdentity,
) {
  let state = &client.frame_navigation_state;
  if let Some(revocation) = state.native_contexts.revoke_verified_deferred(state, browser_id, frame_id, identity) {
    let handler = client.handlers.frame_event_handler.clone();
    let frame_id = frame_id.to_owned();
    crate::native_context::retain_deferred_revocation(revocation.with_delivery(move |origin| {
      origin.browser_state().native_reload.document_lost(&origin);
      if let Some(handler) = handler {
        if let Some(process) = origin.renderer_process_identity() {
          handler(crate::FrameEvent { browser_id, frame_id: frame_id.clone(), is_main: true,
            kind: crate::FrameEventKind::NativeRendererTerminated { origin: origin.clone(), process } });
        }
        handler(crate::FrameEvent { browser_id, frame_id, is_main: true,
          kind: crate::FrameEventKind::NativeContextRevoked { origin } });
      }
    }));
  }
}

/// Pre-mutation native hook: invalidate under the short commit gate only.
/// The retained delivery is process-owned and cannot be discarded by browser
/// destruction or a failed task post. The paired posthook only schedules it.
pub(crate) fn on_native_context_will_revoke<T: UserEvent>(
  client: &TauriCefBrowserClient<T>,
  browser_id: i32,
  frame_id: &str,
  identity: &crate::native_context::VerifiedNativeIdentity,
) {
  let state = &client.frame_navigation_state;
  if let Some(revocation) = state.native_contexts.revoke_verified_deferred(state, browser_id, frame_id, identity) {
    let handler = client.handlers.frame_event_handler.clone();
    let frame_id = frame_id.to_owned();
    crate::native_context::retain_deferred_revocation(revocation.with_delivery(move |origin| {
      if let Some(handler) = handler {
        handler(crate::FrameEvent { browser_id, frame_id, is_main: true,
          kind: crate::FrameEventKind::NativeContextRevoked { origin } });
      }
    }));
  }
}

pub(crate) fn on_native_context_did_revoke() {
  let mut task = DrainNativeRevocations::new();
  if post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task)) != 1 {
    // Runtime shutdown drains the process-owned queue outside CEF callbacks.
    log::error!("native context revocation delivery remains queued until shutdown");
  }
}

wrap_task! {
  struct DrainNativeRevocations;
  impl Task {
    fn execute(&self) { crate::native_context::drain_deferred_revocations(); }
  }
}

/// Revoke only the exact native identity, including a late teardown of an old
/// context; the callback never resolves or relabels the current frame.
pub(crate) fn on_native_context_revoked<T: UserEvent>(
  client: &TauriCefBrowserClient<T>,
  browser: &mut Browser,
  frame_id: &str,
  identity: &crate::native_context::VerifiedNativeIdentity,
) {
  let browser_id = browser.identifier();
  on_native_context_will_revoke(client, browser_id, frame_id, identity);
}

pub(crate) fn on_process_message_received<T: UserEvent>(
  client: &TauriCefBrowserClient<T>,
  browser: Option<&mut Browser>,
  frame: Option<&mut Frame>,
  source_process: ProcessId,
  message: Option<&mut ProcessMessage>,
) -> std::os::raw::c_int {
  if source_process != ProcessId::RENDERER {
    return 0;
  }
  let Some(message) = message else {
    return 0;
  };
  let name = CefString::from(&message.name()).to_string();
  if name == crate::cef_impl::trusted_activation::ACK {
    let (Some(browser), Some(frame), Some(args)) = (browser.as_deref(), frame.as_deref(), message.argument_list()) else { return 1 };
    if frame.is_main() == 0 { return 1; }
    let seed = CefString::from(&args.string(0)).to_string();
    let state = &client.frame_navigation_state;
    if !state.accepts_context_seed(&seed) { return 1; }
    if !state.trusted_activation.acknowledge(browser.identifier(), &seed) {
      state.trusted_activation.fail();
    }
    return 1;
  }
  if CefString::from(&message.name()).to_string() == RENDERER_PID_MESSAGE {
    if let (Some(browser), Some(frame), Some(args)) = (browser, frame, message.argument_list())
      && frame.is_main() != 0
      && let Ok(pid) = CefString::from(&args.string(0)).to_string().parse::<u32>()
      && pid != 0
    {
      crate::frame::emit_frame_event(&client.handlers.frame_event_handler, Some(browser), Some(frame), crate::FrameEventKind::RendererProcessIdentified { pid });
    }
    return 1;
  }
  if CefString::from(&message.name()).to_string() != IPC_MESSAGE_NAME {
    return 0;
  }
  // Protected traffic enters only through the exact-context native pipe.
  if !matches!(client.frame_navigation_state.trusted_init_mode(), Some(crate::TrustedInitMode::Unprotected)) {
    return 1;
  }
  let Some(handler) = client.handlers.ipc_handler.as_ref() else {
    return 1;
  };
  let Some(args) = message.argument_list() else {
    return 1;
  };

  let mut url = CefString::from(&args.string(0)).to_string();
  if url.is_empty()
    && let Some(frame) = frame
  {
    url = CefString::from(&frame.url()).to_string();
  }
  let body = CefString::from(&args.string(1)).to_string();

  if let Ok(request) = http::Request::builder().uri(url).body(body) {
    let Some(position) = client.context.browser_lifetimes.position(client.webview_id, &client.frame_navigation_state.browser_close) else { return 1 };
    let window_id = match position.lock() { Ok(position) => *position, Err(_) => return 1 };
    handler(
      DetachedWebview {
        label: client.label.clone(),
        dispatcher: CefWebviewDispatcher {
          window_id: position.clone(),
          webview_id: client.webview_id,
          context: {
            let mut scoped = client.context.clone();
            let initial = scoped.native_windows.capture(window_id, None, false)
              .and_then(|origin| origin.close()).map(|close| (window_id, close));
            scoped.window_scope = None;
            scoped.webview_scope = Some((client.webview_id, client.frame_navigation_state.browser_close.clone(), position, initial));
            scoped
          },
          browser_close: client.frame_navigation_state.browser_close.clone(),
        },
      },
      request,
    );
  }
  1
}
