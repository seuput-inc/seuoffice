// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Extension traits exposing CEF-specific APIs on [`tauri`] types.
//!
//! The traits are implemented for the statically typed [`CefRuntime`](crate::CefRuntime)
//! and for the type-erased [`tauri::DynRuntime`]. With the latter, the methods fail with
//! [`tauri_runtime::Error::RuntimeTypeMismatch`] when the application is not running on CEF.

use std::sync::Arc;

use tauri::{EventLoopMessage, Manager, Runtime, Webview, WebviewWindow};
use tauri_runtime::dynamic::{DynWebviewAttributes, DynWebviewDispatcher, DynWindowOpener};

use crate::{
  CefWebviewAttributes, CefWebviewDispatcher, ChromeCommandGroup, ConsoleMessage, DevToolsProtocol,
  FrameEvent, NewWindowOpener, RuntimeStyle,
};

type Result<T> = std::result::Result<T, tauri::Error>;

fn not_cef() -> tauri::Error {
  tauri_runtime::Error::RuntimeTypeMismatch(
    "the application is not running on the CEF runtime".into(),
  )
  .into()
}

struct WebviewCompletion(Option<Box<dyn for<'a> FnOnce(Result<&'a crate::Webview>) + Send>>);
impl WebviewCompletion {
  fn finish(mut self, result: Result<&crate::Webview>) {
    if let Some(callback) = self.0.take() { callback(result); }
  }
}
impl Drop for WebviewCompletion {
  fn drop(&mut self) {
    if let Some(callback) = self.0.take() {
      callback(Err(tauri_runtime::Error::FailedToSendMessage.into()));
    }
  }
}

fn dispatch_native_tauri<R: Runtime>(webview: Webview<R>, expected_key: &str, message: crate::native_invoke::NativeTauriMessage) {
  use tauri::ipc::{CallbackFn, InvokeBody, InvokeResponse, InvokeResponseBody};
  let Some(sink) = message.origin.reply_sink() else { return };
  let reject = |reason: &str| {
    if let Ok(bytes) = serde_json::to_vec(reason) { let _ = sink.send(message.error, false, bytes); }
  };
  if !crate::native_context::constant_time_eq(expected_key, &message.invoke_key) {
    reject("Invalid native invoke key.");
    return;
  }
  let Ok(pairs) = serde_json::from_str::<Vec<(String, String)>>(&message.headers_json) else {
    reject("Invalid invoke headers.");
    return;
  };
  let mut headers = http::HeaderMap::new();
  for (name, value) in pairs {
    let (Ok(name), Ok(value)) = (http::header::HeaderName::from_bytes(name.as_bytes()), http::HeaderValue::from_str(&value)) else {
      reject("Invalid invoke headers.");
      return;
    };
    if name.as_str() != crate::native_context::NATIVE_ATTESTATION_HEADER { headers.append(name, value); }
  }
  let body = if message.raw { InvokeBody::Raw(message.body) } else {
    match serde_json::from_slice(&message.body) {
      Ok(value) => InvokeBody::Json(value),
      Err(_) => { reject("Invalid invoke JSON body."); return; }
    }
  };
  let Some(attestation) = message.origin.attest_native_request() else { return };
  let Ok(evidence) = http::HeaderValue::from_str(attestation.header()) else { return };
  headers.insert(crate::native_context::NATIVE_ATTESTATION_HEADER, evidence);
  headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static(
    if message.raw { "application/octet-stream" } else { "application/json" }));
  webview.on_message(tauri::webview::InvokeRequest {
    cmd: message.cmd, callback: CallbackFn(message.callback), error: CallbackFn(message.error),
    url: message.document_url, body, headers, invoke_key: message.invoke_key,
  }, Box::new(move |_, _, response, callback, error| {
    let _attestation = attestation;
    let (callback, raw, bytes) = match response {
      InvokeResponse::Ok(InvokeResponseBody::Raw(bytes)) => (callback.0, true, bytes),
      InvokeResponse::Ok(InvokeResponseBody::Json(json)) => (callback.0, false, json.into_bytes()),
      InvokeResponse::Err(error_value) => match serde_json::to_vec(&error_value.0) {
        Ok(bytes) => (error.0, false, bytes),
        Err(_) => return,
      },
    };
    let _ = sink.send(callback, raw, bytes);
  }));
}

/// Webview dispatchers that may expose the underlying [`CefWebviewDispatcher`].
pub trait AsCefWebviewDispatcher {
  /// Returns the CEF webview dispatcher, if the runtime is CEF.
  fn as_cef_webview_dispatcher(&self) -> Option<&CefWebviewDispatcher<EventLoopMessage>>;
}

impl AsCefWebviewDispatcher for CefWebviewDispatcher<EventLoopMessage> {
  fn as_cef_webview_dispatcher(&self) -> Option<&CefWebviewDispatcher<EventLoopMessage>> {
    Some(self)
  }
}

impl AsCefWebviewDispatcher for DynWebviewDispatcher<EventLoopMessage> {
  fn as_cef_webview_dispatcher(&self) -> Option<&CefWebviewDispatcher<EventLoopMessage>> {
    self.downcast_ref()
  }
}

/// Window openers that may expose the CEF [`NewWindowOpener`].
///
/// Lets a new window handler read the CEF popup source regardless of the runtime generic in use:
///
/// ```rust,no_run
/// use tauri::{WebviewUrl, WebviewWindowBuilder, webview::NewWindowResponse};
/// use tauri_runtime_cef::AsCefWindowOpener;
///
/// tauri::Builder::default()
///   .runtime(tauri_runtime_cef::Cef::default())
///   .setup(|app| {
///     WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
///       .on_new_window(|url, features| {
///         if let Some(opener) = features.opener().as_cef_window_opener() {
///           println!("{url} was opened by {:?}", opener.source_url());
///         }
///         NewWindowResponse::Allow
///       })
///       .build()?;
///     Ok(())
///   });
/// ```
pub trait AsCefWindowOpener {
  /// Returns the CEF window opener, `None` when the opener belongs to another runtime.
  fn as_cef_window_opener(&self) -> Option<&NewWindowOpener>;
}

impl AsCefWindowOpener for NewWindowOpener {
  fn as_cef_window_opener(&self) -> Option<&NewWindowOpener> {
    Some(self)
  }
}

impl AsCefWindowOpener for DynWindowOpener {
  fn as_cef_window_opener(&self) -> Option<&NewWindowOpener> {
    self.downcast_ref()
  }
}

/// Runtime webview attributes that may expose the [`CefWebviewAttributes`].
pub trait AsCefWebviewAttributes {
  /// Returns the CEF attributes, `None` when the attributes belong to another runtime.
  fn as_cef_webview_attributes_mut(&mut self) -> Option<&mut CefWebviewAttributes>;
}

impl AsCefWebviewAttributes for CefWebviewAttributes {
  fn as_cef_webview_attributes_mut(&mut self) -> Option<&mut CefWebviewAttributes> {
    Some(self)
  }
}

impl AsCefWebviewAttributes for DynWebviewAttributes {
  fn as_cef_webview_attributes_mut(&mut self) -> Option<&mut CefWebviewAttributes> {
    self.get_or_default()
  }
}

/// Modifies the CEF attributes of a webview builder, if the builder's attributes are not of another runtime.
fn with_cef_webview_attributes<A: AsCefWebviewAttributes>(
  attributes: &mut A,
  f: impl FnOnce(&mut CefWebviewAttributes),
) {
  match attributes.as_cef_webview_attributes_mut() {
    Some(attributes) => f(attributes),
    None => log::warn!(
      "ignoring the CEF webview attributes: attributes of another runtime were already set on the webview builder"
    ),
  }
}

/// CEF-specific APIs of [`tauri::Webview`] and [`tauri::WebviewWindow`].
pub trait WebviewCefExt {
  /// Send a message to the DevTools agent. The message should be a UTF-8 encoded JSON
  /// string following the Chrome DevTools Protocol format.
  ///
  /// Callers share one native request identifier space on this browser, so the
  /// message's `id` must come from
  /// [`allocate_devtools_message_id`](crate::allocate_devtools_message_id).
  /// A hardcoded or self-incremented `id` can collide with a request another
  /// caller already sent, which consumes that producer's
  /// [`DevToolsProtocol::MethodResult`]. The runtime's own requests are issued
  /// from a reserved range the public allocator never returns, so they cannot
  /// be consumed this way.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use tauri::Manager;
  /// use tauri_runtime_cef::{WebviewCefExt, allocate_devtools_message_id};
  ///
  /// tauri::Builder::default()
  ///   .runtime(tauri_runtime_cef::Cef::default())
  ///   .setup(|app| {
  ///     let webview = app.get_webview_window("main").unwrap();
  ///     // Enable Page domain to receive page lifecycle events
  ///     let message_id = allocate_devtools_message_id()?;
  ///     let msg = format!(r#"{{"id":{message_id},"method":"Page.enable","params":{{}}}}"#);
  ///     webview.send_dev_tools_message(msg.as_bytes())?;
  ///     Ok(())
  ///   });
  /// ```
  fn send_dev_tools_message(&self, message: &[u8]) -> Result<()>;

  /// Enqueues a protocol message without blocking the UI thread.
  /// Completion reports native dispatch, not the protocol method result.
  /// It runs on the CEF UI thread after dispatch, or on the calling thread if
  /// enqueue fails. A closed view completes with an error.
  fn send_dev_tools_message_async<C: FnOnce(Result<()>) + Send + 'static>(
    &self, message: &[u8], completion: C,
  ) -> Result<()>;

  /// Reports when initial navigation has been dispatched, not when it commits.
  /// Errors if startup or the view fails before that point; never blocks UI.
  /// Run the short owner fence at the actual native protocol send, not enqueue.
  fn send_dev_tools_message_authorized<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, message: &[u8], admit: crate::NativeProtocolAdmission, completion: C,
  ) -> Result<()>;

  fn when_navigation_ready<C: FnOnce(Result<()>) + Send + 'static>(&self, completion: C) -> Result<()>;

  /// Signals owner readiness for an unprotected explicit-start browser.
  /// No navigation or protected bridge is created by this operation.
  fn activate_unprotected_initial_navigation<C: FnOnce(Result<()>) + Send + 'static>(&self, completion: C) -> Result<()>;

  /// Starts an unprotected navigation and asynchronously returns its exact native document.
  fn navigate_observed<C: FnOnce(std::result::Result<crate::NativeObservedDocument, String>) + Send + 'static>(
    &self, url: url::Url, completion: C,
  ) -> Result<crate::NativeObservedNavigation>;

  /// Reparent between captured original native window creations.
  fn reparent_native_exact<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, source: &crate::NativeWindowHandle, target: &crate::NativeWindowHandle, completion: C,
  ) -> Result<()>;

  /// Complete only after the original native child's own visibility is observed.
  fn set_native_visible_checked<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, visible: bool, completion: C,
  ) -> Result<()>;

  /// Browser-scoped pre-navigation setup; not an exact-document protocol port.
  fn send_browser_setup_protocol<C: FnOnce(std::result::Result<Vec<u8>, String>) + Send + 'static>(
    &self, expected: crate::FrameNavigationState, method: String, params: serde_json::Value, completion: C,
  ) -> std::result::Result<crate::NativeBrowserProtocol, String>;

  /// The exact creation reservation, available before native readiness.
  fn native_browser_close(&self) -> Result<crate::NativeBrowserClose>;

  fn reload_native_reserved<C: FnOnce(std::result::Result<crate::NativeContextToken, String>) + Send + 'static>(
    &self, reservation: crate::NativeReloadReservation, admission: crate::NativeReloadAdmission, completion: C,
  ) -> Result<()>;

  /// Register a callback to receive DevTools protocol messages. Messages include
  /// both method results and events from the DevTools agent.
  ///
  /// The callback observes the whole browser, including requests the runtime and
  /// other callers sent. Match [`DevToolsProtocol::MethodResult`] against an
  /// identifier obtained from
  /// [`allocate_devtools_message_id`](crate::allocate_devtools_message_id)
  /// instead of assuming every result belongs to this observer.
  ///
  /// It is scoped to this webview's own native browser, so a CEF-owned popup is
  /// a separate browser whose protocol traffic — its page content, its network
  /// activity and its dialog messages — is never reported here; observe popups
  /// through [`Webview::popups`](crate::Webview::popups).
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use tauri::Manager;
  /// use tauri_runtime_cef::{DevToolsProtocol, WebviewCefExt, allocate_devtools_message_id};
  ///
  /// tauri::Builder::default()
  ///   .runtime(tauri_runtime_cef::Cef::default())
  ///   .setup(|app| {
  ///     let webview = app.get_webview_window("main").unwrap();
  ///     let message_id = allocate_devtools_message_id()?;
  ///     webview.on_dev_tools_protocol(move |protocol| {
  ///       match protocol {
  ///         DevToolsProtocol::Message(msg) => {
  ///           if let Ok(s) = std::str::from_utf8(&msg) {
  ///             println!("DevTools message: {}", s);
  ///           }
  ///         }
  ///         DevToolsProtocol::Event { method, params } => {
  ///           println!("DevTools event: {} {:?}", method, params);
  ///         }
  ///         // Only this result answers the request sent below.
  ///         DevToolsProtocol::MethodResult { message_id: id, success, .. } if id == message_id => {
  ///           println!("Page.enable success={}", success);
  ///         }
  ///         DevToolsProtocol::MethodResult { .. } => {}
  ///       }
  ///     })?;
  ///     let msg = format!(r#"{{"id":{message_id},"method":"Page.enable","params":{{}}}}"#);
  ///     webview.send_dev_tools_message(msg.as_bytes())?;
  ///     Ok(())
  ///   });
  /// ```
  fn on_dev_tools_protocol<F: Fn(DevToolsProtocol) + Send + Sync + 'static>(
    &self,
    f: F,
  ) -> Result<()>;

  /// Registers an observer without blocking the UI thread.
  /// Completion follows the same dispatch and failure rules as
  /// `send_dev_tools_message_async`.
  fn on_dev_tools_protocol_async<F, C>(&self, handler: F, completion: C) -> Result<()>
  where
    F: Fn(DevToolsProtocol) + Send + Sync + 'static,
    C: FnOnce(Result<()>) + Send + 'static;

  /// Executes a closure with the CEF platform webview handle, on the CEF UI thread.
  ///
  /// See [`crate::Webview`] for the native state it exposes, which is sampled
  /// immediately before the closure runs and is not refreshed afterwards.
  fn with_cef_webview<F: FnOnce(&crate::Webview) + Send + 'static>(&self, f: F) -> Result<()>;

  /// Samples the native webview on the UI thread, or reports a closed view or
  /// type mismatch exactly once. Enqueueing never waits for the UI thread.
  fn with_cef_webview_async<F: for<'a> FnOnce(Result<&'a crate::Webview>) + Send + 'static>(&self, f: F) -> Result<()>;

  /// Bind an actual native browser to its immutable application owner before
  /// approving the first protected navigation. The registration callback must
  /// perform only short exact-owner binding, never UI/core waits.
  fn bind_native_context_ipc<F, S>(&self, on_context: F, on_sync: S) -> Result<()>
  where
    F: Fn(&crate::NativeContextToken) -> bool + Send + Sync + 'static,
    S: Fn(crate::NativeSyncRequest) + Send + Sync + 'static;
}

impl<R: Runtime> WebviewCefExt for Webview<R>
where
  R::WebviewDispatcher: AsCefWebviewDispatcher,
{
  fn send_dev_tools_message(&self, message: &[u8]) -> Result<()> {
    self
      .dispatcher()
      .as_cef_webview_dispatcher()
      .ok_or_else(not_cef)?
      .send_dev_tools_message(message)
      .map_err(Into::into)
  }

  fn send_dev_tools_message_async<C: FnOnce(Result<()>) + Send + 'static>(
    &self, message: &[u8], completion: C,
  ) -> Result<()> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .send_dev_tools_message_async(message, move |result| completion(result.map_err(Into::into)))
      .map_err(Into::into)
  }

  fn send_dev_tools_message_authorized<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, message: &[u8], admit: crate::NativeProtocolAdmission, completion: C,
  ) -> Result<()> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .send_dev_tools_message_authorized(message, admit, completion).map_err(Into::into)
  }

  fn when_navigation_ready<C: FnOnce(Result<()>) + Send + 'static>(&self, completion: C) -> Result<()> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .when_navigation_ready(move |result| completion(result.map_err(Into::into)))
      .map_err(Into::into)
  }

  fn activate_unprotected_initial_navigation<C: FnOnce(Result<()>) + Send + 'static>(&self, completion: C) -> Result<()> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .activate_unprotected_initial_navigation(move |result| completion(result.map_err(Into::into)))
      .map_err(Into::into)
  }

  fn navigate_observed<C: FnOnce(std::result::Result<crate::NativeObservedDocument, String>) + Send + 'static>(
    &self, url: url::Url, completion: C,
  ) -> Result<crate::NativeObservedNavigation> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .navigate_observed(url, completion).map_err(Into::into)
  }

  fn reparent_native_exact<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, source: &crate::NativeWindowHandle, target: &crate::NativeWindowHandle, completion: C,
  ) -> Result<()> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .reparent_native_exact(source, target, completion).map_err(Into::into)
  }

  fn set_native_visible_checked<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, visible: bool, completion: C,
  ) -> Result<()> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .set_native_visible_checked(visible, completion).map_err(Into::into)
  }

  fn send_browser_setup_protocol<C: FnOnce(std::result::Result<Vec<u8>, String>) + Send + 'static>(
    &self, expected: crate::FrameNavigationState, method: String, params: serde_json::Value, completion: C,
  ) -> std::result::Result<crate::NativeBrowserProtocol, String> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(|| "Not a CEF webview.".to_owned())?
      .send_browser_setup_protocol(expected, method, params, completion)
  }

  fn native_browser_close(&self) -> Result<crate::NativeBrowserClose> {
    Ok(self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?.browser_close())
  }

  fn reload_native_reserved<C: FnOnce(std::result::Result<crate::NativeContextToken, String>) + Send + 'static>(
    &self, reservation: crate::NativeReloadReservation, admission: crate::NativeReloadAdmission, completion: C,
  ) -> Result<()> {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .reload_native_reserved(reservation, admission, completion).map_err(Into::into)
  }

  fn on_dev_tools_protocol<F: Fn(DevToolsProtocol) + Send + Sync + 'static>(
    &self,
    f: F,
  ) -> Result<()> {
    self
      .dispatcher()
      .as_cef_webview_dispatcher()
      .ok_or_else(not_cef)?
      .on_dev_tools_protocol(f)
      .map_err(Into::into)
  }

  fn on_dev_tools_protocol_async<F, C>(&self, handler: F, completion: C) -> Result<()>
  where
    F: Fn(DevToolsProtocol) + Send + Sync + 'static,
    C: FnOnce(Result<()>) + Send + 'static,
  {
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .on_dev_tools_protocol_async(handler, move |result| completion(result.map_err(Into::into)))
      .map_err(Into::into)
  }

  fn with_cef_webview<F: FnOnce(&crate::Webview) + Send + 'static>(&self, f: F) -> Result<()> {
    if self.dispatcher().as_cef_webview_dispatcher().is_none() {
      return Err(not_cef());
    }
    self.with_webview(move |webview| {
      if let Some(webview) = webview.downcast_ref::<crate::Webview>() {
        f(webview)
      }
    })
  }

  fn with_cef_webview_async<F: for<'a> FnOnce(Result<&'a crate::Webview>) + Send + 'static>(&self, f: F) -> Result<()> {
    let completion = WebviewCompletion(Some(Box::new(f)));
    if self.dispatcher().as_cef_webview_dispatcher().is_none() {
      completion.finish(Err(not_cef()));
      return Err(not_cef());
    }
    self.with_webview(move |webview| {
      completion.finish(webview.downcast_ref::<crate::Webview>().ok_or_else(not_cef));
    })
  }

  fn bind_native_context_ipc<F, S>(&self, on_context: F, on_sync: S) -> Result<()>
  where
    F: Fn(&crate::NativeContextToken) -> bool + Send + Sync + 'static,
    S: Fn(crate::NativeSyncRequest) + Send + Sync + 'static,
  {
    let webview = self.clone();
    let key = self.app_handle().invoke_key().to_owned();
    let target = Arc::new(crate::native_invoke::NativeInvocationTarget {
      tauri: Arc::new(move |message| dispatch_native_tauri(webview.clone(), &key, message)),
      registered: Arc::new(on_context),
      sync: Arc::new(on_sync),
    });
    self.dispatcher().as_cef_webview_dispatcher().ok_or_else(not_cef)?
      .bind_native_context_target(target).map_err(Into::into)
  }
}

impl<R: Runtime> WebviewCefExt for WebviewWindow<R>
where
  R::WebviewDispatcher: AsCefWebviewDispatcher,
{
  fn send_dev_tools_message(&self, message: &[u8]) -> Result<()> {
    self.as_ref().send_dev_tools_message(message)
  }

  fn send_dev_tools_message_async<C: FnOnce(Result<()>) + Send + 'static>(
    &self, message: &[u8], completion: C,
  ) -> Result<()> {
    self.as_ref().send_dev_tools_message_async(message, completion)
  }

  fn send_dev_tools_message_authorized<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, message: &[u8], admit: crate::NativeProtocolAdmission, completion: C,
  ) -> Result<()> {
    self.as_ref().send_dev_tools_message_authorized(message, admit, completion)
  }

  fn when_navigation_ready<C: FnOnce(Result<()>) + Send + 'static>(&self, completion: C) -> Result<()> {
    self.as_ref().when_navigation_ready(completion)
  }

  fn activate_unprotected_initial_navigation<C: FnOnce(Result<()>) + Send + 'static>(&self, completion: C) -> Result<()> {
    self.as_ref().activate_unprotected_initial_navigation(completion)
  }

  fn navigate_observed<C: FnOnce(std::result::Result<crate::NativeObservedDocument, String>) + Send + 'static>(
    &self, url: url::Url, completion: C,
  ) -> Result<crate::NativeObservedNavigation> {
    self.as_ref().navigate_observed(url, completion)
  }

  fn reparent_native_exact<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, source: &crate::NativeWindowHandle, target: &crate::NativeWindowHandle, completion: C,
  ) -> Result<()> {
    self.as_ref().reparent_native_exact(source, target, completion)
  }

  fn set_native_visible_checked<C: FnOnce(std::result::Result<(), String>) + Send + 'static>(
    &self, visible: bool, completion: C,
  ) -> Result<()> {
    self.as_ref().set_native_visible_checked(visible, completion)
  }

  fn send_browser_setup_protocol<C: FnOnce(std::result::Result<Vec<u8>, String>) + Send + 'static>(
    &self, expected: crate::FrameNavigationState, method: String, params: serde_json::Value, completion: C,
  ) -> std::result::Result<crate::NativeBrowserProtocol, String> {
    self.as_ref().send_browser_setup_protocol(expected, method, params, completion)
  }

  fn native_browser_close(&self) -> Result<crate::NativeBrowserClose> {
    self.as_ref().native_browser_close()
  }

  fn reload_native_reserved<C: FnOnce(std::result::Result<crate::NativeContextToken, String>) + Send + 'static>(
    &self, reservation: crate::NativeReloadReservation, admission: crate::NativeReloadAdmission, completion: C,
  ) -> Result<()> {
    self.as_ref().reload_native_reserved(reservation, admission, completion)
  }

  fn on_dev_tools_protocol<F: Fn(DevToolsProtocol) + Send + Sync + 'static>(
    &self,
    f: F,
  ) -> Result<()> {
    self.as_ref().on_dev_tools_protocol(f)
  }


  fn on_dev_tools_protocol_async<F, C>(&self, handler: F, completion: C) -> Result<()>
  where
    F: Fn(DevToolsProtocol) + Send + Sync + 'static,
    C: FnOnce(Result<()>) + Send + 'static,
  {
    self.as_ref().on_dev_tools_protocol_async(handler, completion)
  }
  fn with_cef_webview<F: FnOnce(&crate::Webview) + Send + 'static>(&self, f: F) -> Result<()> {
    self.as_ref().with_cef_webview(f)
  }

  fn with_cef_webview_async<F: for<'a> FnOnce(Result<&'a crate::Webview>) + Send + 'static>(&self, f: F) -> Result<()> {
    self.as_ref().with_cef_webview_async(f)
  }

  fn bind_native_context_ipc<F, S>(&self, on_context: F, on_sync: S) -> Result<()>
  where
    F: Fn(&crate::NativeContextToken) -> bool + Send + Sync + 'static,
    S: Fn(crate::NativeSyncRequest) + Send + Sync + 'static,
  {
    self.as_ref().bind_native_context_ipc(on_context, on_sync)
  }
}

/// Runtime builder adapters for a pre-allocation native window reservation.
#[cfg(feature = "unstable")]
pub trait AsCefWindowBuilder {
  fn install_native_window_submission(&mut self, guard: crate::NativeWindowSubmissionGuard);
}

#[cfg(feature = "unstable")]
impl AsCefWindowBuilder for crate::WindowBuilderWrapper {
  fn install_native_window_submission(&mut self, guard: crate::NativeWindowSubmissionGuard) {
    self.attrs.window_submission = Some(guard.submit());
  }
}

#[cfg(feature = "unstable")]
impl AsCefWindowBuilder for tauri_runtime::dynamic::DynWindowBuilder {
  fn install_native_window_submission(&mut self, guard: crate::NativeWindowSubmissionGuard) {
    let submission = guard.submit();
    *self = std::mem::take(self).customize(move |builder| {
      if let Some(builder) = builder.downcast_mut::<crate::WindowBuilderWrapper>() {
        builder.attrs.window_submission = Some(submission.clone());
      }
    });
  }
}

/// CEF-specific pre-allocation configuration for a bare Tauri window.
#[cfg(feature = "unstable")]
pub trait WindowBuilderCefExt {
  #[must_use]
  fn native_window_submission(self, guard: crate::NativeWindowSubmissionGuard) -> Self;
}

#[cfg(feature = "unstable")]
impl<'a, R: Runtime, M: Manager<R>> WindowBuilderCefExt for tauri::window::WindowBuilder<'a, R, M>
where
  <R::WindowDispatcher as tauri_runtime::WindowDispatch<EventLoopMessage>>::WindowBuilder: AsCefWindowBuilder,
{
  fn native_window_submission(mut self, guard: crate::NativeWindowSubmissionGuard) -> Self {
    self.runtime_window_builder_mut().install_native_window_submission(guard);
    self
  }
}

/// CEF-specific APIs of [`tauri::WebviewWindowBuilder`].
pub trait WebviewWindowBuilderCefExt {
  /// Sets the browser runtime style.
  ///
  /// See [`RuntimeStyle`] for more information.
  #[must_use]
  fn browser_runtime_style(self, style: RuntimeStyle) -> Self;

  /// Sets the native trusted document-start initialization contract.
  #[must_use]
  fn trusted_init_mode(self, mode: crate::TrustedInitMode) -> Self;

  #[must_use]
  fn protected_navigation_start(self, mode: crate::ProtectedNavigationStart) -> Self;

  #[must_use]
  fn network_session(self, session: crate::NativeNetworkSession) -> Self;

  /// Transfers an owned pre-dispatch creation reservation into this builder.
  #[must_use]
  fn native_browser_submission(self, submission: crate::NativeBrowserSubmissionGuard) -> Self;

  /// Transfer an exact original-window creation reservation.
  #[must_use]
  fn native_window_submission(self, submission: crate::NativeWindowSubmissionGuard) -> Self;

  /// Observes native CEF lifecycle events for main and child frames.
  ///
  /// The callback runs synchronously on CEF's UI thread. It must return
  /// promptly and must not wait for an event-loop operation. This observer
  /// does not replace the navigation policy configured by `on_navigation`.
  /// It is scoped to this webview's own native browser, so a CEF-owned popup
  /// is a separate browser that is never reported here — observe popups
  /// through [`Webview::popups`](crate::Webview::popups).
  #[must_use]
  fn on_frame_event<F: Fn(FrameEvent) + Send + Sync + 'static>(self, handler: F) -> Self;

  /// Allows or cancels each resource request before CEF loads it.
  /// The callback runs on a CEF IO thread and must return promptly.
  #[must_use]
  fn on_resource_request<F: Fn(&str) -> bool + Send + Sync + 'static>(self, handler: F) -> Self;

  #[must_use]
  fn on_browser_resource_request<F: Fn(&crate::BrowserResourceRequest) -> bool + Send + Sync + 'static>(self, handler: F) -> Self;

  /// Observes focus and HTML fullscreen changes for this webview's browser.
  /// Runs on the CEF UI thread and must return promptly.
  #[must_use]
  fn on_view_event<F: Fn(crate::FrameNavigationState, crate::ViewEvent) + Send + Sync + 'static>(self, handler: F) -> Self;

  /// Observes the messages the renderer writes to the JavaScript console,
  /// without DevTools having to be open.
  ///
  /// The callback runs synchronously on CEF's UI thread, so it must return
  /// promptly and must not wait for an event-loop operation. Observing a message
  /// does not suppress it: CEF logs it as it normally would. It is scoped to this
  /// webview's own native browser, so neither a CEF-owned popup's output nor that
  /// of a DevTools window opened on this webview is reported here.
  #[must_use]
  fn on_console_message<F: Fn(ConsoleMessage) + Send + Sync + 'static>(self, handler: F) -> Self;

  /// Keeps the named families of Chrome commands rather than swallowing them.
  ///
  /// A Chrome style browser keeps its whole accelerator table live even hosted as a
  /// child view with no browser UI, so by default this runtime swallows the commands
  /// that have no meaning in an app window — new window and tab, the tab strip,
  /// history, downloads and settings, print, save page, view source, the omnibox
  /// focus commands. Naming a [`ChromeCommandGroup`] here lets that family run the
  /// way it would in a browser. Calling this more than once replaces the previous
  /// list.
  ///
  /// DevTools and zoom accelerators are not covered here: they follow
  /// `WebviewAttributes::devtools` and `WebviewAttributes::zoom_hotkeys_enabled`.
  ///
  /// ```no_run
  /// # use tauri_runtime_cef::{AsCefWebviewAttributes, ChromeCommandGroup};
  /// # fn f<R, M>(builder: tauri::WebviewWindowBuilder<'_, R, M>)
  /// # where
  /// #   R: tauri::Runtime,
  /// #   M: tauri::Manager<R>,
  /// #   R::RuntimeWebviewAttributes: AsCefWebviewAttributes,
  /// # {
  /// use tauri_runtime_cef::WebviewWindowBuilderCefExt;
  /// // Ctrl+P prints and Alt+Left goes back, as a user expects.
  /// builder.allow_chrome_commands([ChromeCommandGroup::Document, ChromeCommandGroup::History]);
  /// # }
  /// ```
  #[must_use]
  fn allow_chrome_commands<I: IntoIterator<Item = ChromeCommandGroup>>(self, groups: I) -> Self;

  /// Takes a last look at the CEF [`BrowserSettings`](cef::BrowserSettings) before the
  /// browser is created.
  ///
  /// The runtime maps the portable `WebviewAttributes` onto these settings first, so this
  /// can change what it decided as well as reach the fields Tauri has no attribute for:
  /// the font families and sizes, `remote_fonts`, `local_storage`, `databases`, `webgl`,
  /// `tab_to_links`, `javascript_dom_paste` and `default_encoding`.
  ///
  /// ```no_run
  /// # use tauri_runtime_cef::AsCefWebviewAttributes;
  /// # fn f<R, M>(builder: tauri::WebviewWindowBuilder<'_, R, M>)
  /// # where
  /// #   R: tauri::Runtime,
  /// #   M: tauri::Manager<R>,
  /// #   R::RuntimeWebviewAttributes: AsCefWebviewAttributes,
  /// # {
  /// use tauri_runtime_cef::WebviewWindowBuilderCefExt;
  /// use tauri_runtime_cef::cef::{State, sys::cef_state_t};
  ///
  /// // An app that ships its own fonts has no use for the ones a page asks for.
  /// builder.with_browser_settings(|settings| {
  ///   settings.remote_fonts = State::from(cef_state_t::STATE_DISABLED);
  /// });
  /// # }
  /// ```
  #[must_use]
  fn with_browser_settings<F: Fn(&mut cef::BrowserSettings) + Send + Sync + 'static>(
    self,
    callback: F,
  ) -> Self;
}

impl<'a, R: Runtime, M: Manager<R>> WebviewWindowBuilderCefExt
  for tauri::WebviewWindowBuilder<'a, R, M>
where
  R::RuntimeWebviewAttributes: AsCefWebviewAttributes,
{
  fn browser_runtime_style(mut self, style: RuntimeStyle) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.runtime_style = Some(style);
    });
    self
  }

  fn trusted_init_mode(mut self, mode: crate::TrustedInitMode) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.trusted_init_mode = mode;
    });
    self
  }

  fn protected_navigation_start(mut self, mode: crate::ProtectedNavigationStart) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.protected_navigation_start = mode;
    });
    self
  }

  fn network_session(mut self, session: crate::NativeNetworkSession) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.network_session = Some(session);
    });
    self
  }

  fn native_browser_submission(mut self, submission: crate::NativeBrowserSubmissionGuard) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.browser_submission = Some(submission.into_submission());
    });
    self
  }

  fn native_window_submission(mut self, submission: crate::NativeWindowSubmissionGuard) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.window_submission = Some(submission.submit());
    });
    self
  }

  fn on_frame_event<F: Fn(FrameEvent) + Send + Sync + 'static>(mut self, handler: F) -> Self {
    let handler = Arc::new(handler);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.frame_event_handler = Some(handler);
    });
    self
  }

  fn on_resource_request<F: Fn(&str) -> bool + Send + Sync + 'static>(mut self, handler: F) -> Self {
    let handler = Arc::new(handler);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.resource_request_handler = Some(handler.clone());
    });
    self
  }

  fn on_browser_resource_request<F: Fn(&crate::BrowserResourceRequest) -> bool + Send + Sync + 'static>(mut self, handler: F) -> Self {
    let handler = Arc::new(handler);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.browser_resource_request_handler = Some(handler);
    });
    self
  }

  fn on_view_event<F: Fn(crate::FrameNavigationState, crate::ViewEvent) + Send + Sync + 'static>(mut self, handler: F) -> Self {
    let handler = Arc::new(handler);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.view_event_handler = Some(handler);
    });
    self
  }

  fn on_console_message<F: Fn(ConsoleMessage) + Send + Sync + 'static>(
    mut self,
    handler: F,
  ) -> Self {
    let handler = Arc::new(handler);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.console_message_handler = Some(handler);
    });
    self
  }

  fn allow_chrome_commands<I: IntoIterator<Item = ChromeCommandGroup>>(
    mut self,
    groups: I,
  ) -> Self {
    let groups = groups.into_iter().collect::<Vec<_>>();
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.allowed_chrome_commands = groups.clone();
    });
    self
  }

  fn with_browser_settings<F: Fn(&mut cef::BrowserSettings) + Send + Sync + 'static>(
    mut self,
    callback: F,
  ) -> Self {
    let callback = Arc::new(callback);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.browser_settings_callback = Some(callback.clone());
    });
    self
  }
}

/// CEF-specific APIs of [`tauri::webview::WebviewBuilder`].
#[cfg(feature = "unstable")]
pub trait WebviewBuilderCefExt {
  /// Sets the browser runtime style.
  ///
  /// See [`RuntimeStyle`] for more information.
  #[must_use]
  fn browser_runtime_style(self, style: RuntimeStyle) -> Self;

  /// Sets the native trusted document-start initialization contract.
  #[must_use]
  fn trusted_init_mode(self, mode: crate::TrustedInitMode) -> Self;

  #[must_use]
  fn protected_navigation_start(self, mode: crate::ProtectedNavigationStart) -> Self;

  #[must_use]
  fn network_session(self, session: crate::NativeNetworkSession) -> Self;

  /// Supplies the exact pre-dispatch native-browser creation/close receipt.
  #[must_use]
  fn native_browser_close_reservation(self, close: crate::NativeBrowserClose) -> Self;

  /// Transfers an owned pre-dispatch creation reservation into this builder.
  #[must_use]
  fn native_browser_submission(self, submission: crate::NativeBrowserSubmissionGuard) -> Self;

  /// Observes native CEF lifecycle events for main and child frames.
  ///
  /// The callback runs synchronously on CEF's UI thread. It must return
  /// promptly and must not wait for an event-loop operation. This observer
  /// does not replace the navigation policy configured by `on_navigation`.
  /// It is scoped to this webview's own native browser, so a CEF-owned popup
  /// is a separate browser that is never reported here — observe popups
  /// through [`Webview::popups`](crate::Webview::popups).
  #[must_use]
  fn on_frame_event<F: Fn(FrameEvent) + Send + Sync + 'static>(self, handler: F) -> Self;

  /// Observes focus and HTML fullscreen changes for this webview's browser.
  /// Runs on the CEF UI thread and must return promptly.
  #[must_use]
  fn on_view_event<F: Fn(crate::FrameNavigationState, crate::ViewEvent) + Send + Sync + 'static>(self, handler: F) -> Self;

  /// Observes the messages the renderer writes to the JavaScript console,
  /// without DevTools having to be open.
  ///
  /// The callback runs synchronously on CEF's UI thread, so it must return
  /// promptly and must not wait for an event-loop operation. Observing a message
  /// does not suppress it: CEF logs it as it normally would. It is scoped to this
  /// webview's own native browser, so neither a CEF-owned popup's output nor that
  /// of a DevTools window opened on this webview is reported here.
  #[must_use]
  fn on_console_message<F: Fn(ConsoleMessage) + Send + Sync + 'static>(self, handler: F) -> Self;

  /// Keeps the named families of Chrome commands rather than swallowing them.
  ///
  /// A Chrome style browser keeps its whole accelerator table live even hosted as a
  /// child view with no browser UI, so by default this runtime swallows the commands
  /// that have no meaning in an app window — new window and tab, the tab strip,
  /// history, downloads and settings, print, save page, view source, the omnibox
  /// focus commands. Naming a [`ChromeCommandGroup`] here lets that family run the
  /// way it would in a browser. Calling this more than once replaces the previous
  /// list.
  ///
  /// DevTools and zoom accelerators are not covered here: they follow
  /// `WebviewAttributes::devtools` and `WebviewAttributes::zoom_hotkeys_enabled`.
  #[must_use]
  fn allow_chrome_commands<I: IntoIterator<Item = ChromeCommandGroup>>(self, groups: I) -> Self;

  /// Takes a last look at the CEF [`BrowserSettings`](cef::BrowserSettings) before the
  /// browser is created.
  ///
  /// The runtime maps the portable `WebviewAttributes` onto these settings first, so this
  /// can change what it decided as well as reach the fields Tauri has no attribute for:
  /// the font families and sizes, `remote_fonts`, `local_storage`, `databases`, `webgl`,
  /// `tab_to_links`, `javascript_dom_paste` and `default_encoding`.
  #[must_use]
  fn with_browser_settings<F: Fn(&mut cef::BrowserSettings) + Send + Sync + 'static>(
    self,
    callback: F,
  ) -> Self;
}

#[cfg(feature = "unstable")]
impl<R: Runtime> WebviewBuilderCefExt for tauri::webview::WebviewBuilder<R>
where
  R::RuntimeWebviewAttributes: AsCefWebviewAttributes,
{
  fn browser_runtime_style(mut self, style: RuntimeStyle) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.runtime_style = Some(style);
    });
    self
  }

  fn trusted_init_mode(mut self, mode: crate::TrustedInitMode) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.trusted_init_mode = mode;
    });
    self
  }

  fn protected_navigation_start(mut self, mode: crate::ProtectedNavigationStart) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.protected_navigation_start = mode;
    });
    self
  }

  fn network_session(mut self, session: crate::NativeNetworkSession) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.network_session = Some(session);
    });
    self
  }

  fn native_browser_close_reservation(mut self, close: crate::NativeBrowserClose) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.browser_submission = Some(crate::browser_close::NativeBrowserSubmission::new(close));
    });
    self
  }

  fn native_browser_submission(mut self, submission: crate::NativeBrowserSubmissionGuard) -> Self {
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.browser_submission = Some(submission.into_submission());
    });
    self
  }

  fn on_frame_event<F: Fn(FrameEvent) + Send + Sync + 'static>(mut self, handler: F) -> Self {
    let handler = Arc::new(handler);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.frame_event_handler = Some(handler);
    });
    self
  }

  fn on_view_event<F: Fn(crate::FrameNavigationState, crate::ViewEvent) + Send + Sync + 'static>(mut self, handler: F) -> Self {
    let handler = Arc::new(handler);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.view_event_handler = Some(handler);
    });
    self
  }

  fn on_console_message<F: Fn(ConsoleMessage) + Send + Sync + 'static>(
    mut self,
    handler: F,
  ) -> Self {
    let handler = Arc::new(handler);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.console_message_handler = Some(handler);
    });
    self
  }

  fn allow_chrome_commands<I: IntoIterator<Item = ChromeCommandGroup>>(
    mut self,
    groups: I,
  ) -> Self {
    let groups = groups.into_iter().collect::<Vec<_>>();
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.allowed_chrome_commands = groups.clone();
    });
    self
  }

  fn with_browser_settings<F: Fn(&mut cef::BrowserSettings) + Send + Sync + 'static>(
    mut self,
    callback: F,
  ) -> Self {
    let callback = Arc::new(callback);
    with_cef_webview_attributes(self.runtime_specific_attributes_mut(), |attributes| {
      attributes.browser_settings_callback = Some(callback.clone());
    });
    self
  }
}

#[cfg(test)]
mod completion_tests {
  use super::*;
  use std::sync::Mutex;

  #[test]
  fn dropped_and_failed_webview_callbacks_complete_once() {
    let results = Arc::new(Mutex::new(Vec::new()));
    let observed = results.clone();
    drop(WebviewCompletion(Some(Box::new(move |result| observed.lock().unwrap().push(result.is_err())))));
    let observed = results.clone();
    WebviewCompletion(Some(Box::new(move |result| observed.lock().unwrap().push(result.is_err()))))
      .finish(Err(not_cef()));
    assert_eq!(*results.lock().unwrap(), vec![true, true]);
  }
}
