// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex, Weak, mpsc::Sender};

use cef::*;
use tauri_runtime::{UserEvent, window::WindowId};

use crate::{
  cef_impl::{ipc, request_handler},
  macros::wrap_with_args,
  runtime::{CefRuntime, Message, RuntimeContext},
};

mod command;
mod context_menu;
mod display;
mod download;
mod drag;
mod frame;
mod focus;
mod keyboard;
mod life_span;
mod load;
mod permission;
mod process;

use command::{TauriCefCommandHandler, TauriCefCommandHandlerArgs};
use context_menu::TauriCefContextMenuHandler;
use display::{TauriCefDisplayHandler, TauriCefDisplayHandlerArgs};
use download::TauriCefDownloadHandler;
use drag::TauriCefDragHandler;
pub(crate) use drag::{
  DragDropEventTarget, DragDropScriptEvent, DragDropState, WebDragDropResourceRequestHandler,
  WebDragDropResourceRequestHandlerArgs, drag_drop_initialization_script, event_from_script_event,
};
use keyboard::TauriCefKeyboardHandler;
use life_span::{TauriCefChildLifeSpanHandler, TauriCefChildLifeSpanHandlerArgs};
pub(crate) use life_span::check_and_reload_if_blank;
use load::TauriCefLoadHandler;
pub(crate) use permission::PermissionRequestHandler;
use permission::TauriCefPermissionHandler;
pub(crate) use process::TauriCefBrowserProcessHandler;

pub(crate) struct TauriCefBrowserClientHandlers<T: UserEvent> {
  pub(crate) frame_event_handler: Option<Arc<crate::FrameEventHandler>>,
  pub(crate) view_event_handler: Option<Arc<crate::ViewEventHandler>>,
  pub(crate) ipc_handler: Option<Arc<ipc::IpcHandler<T>>>,
  pub(crate) on_page_load_handler: Option<Arc<tauri_runtime::webview::OnPageLoadHandler>>,
  pub(crate) document_title_changed_handler:
    Option<Arc<tauri_runtime::webview::DocumentTitleChangedHandler>>,
  pub(crate) navigation_handler: Option<Arc<tauri_runtime::webview::NavigationHandler>>,
  pub(crate) resource_request_handler: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>,
  pub(crate) browser_request_policy: Option<Arc<crate::browser_request::BrowserRequestPolicy>>,
  pub(crate) session_request_policy: Option<crate::network_session::NetworkSessionPolicy>,
  pub(crate) new_window_handler:
    Option<Arc<tauri_runtime::webview::NewWindowHandler<T, CefRuntime<T>>>>,
  pub(crate) download_handler: Option<Arc<tauri_runtime::webview::DownloadHandler>>,
  pub(crate) console_message_handler: Option<Arc<crate::ConsoleMessageHandler>>,
  pub(crate) permission_request_handler: Option<Arc<PermissionRequestHandler>>,
  pub(crate) web_content_process_terminate_handler:
    Option<Arc<tauri_runtime::webview::OnWebContentProcessTerminateHandler>>,
}

impl<T: UserEvent> Clone for TauriCefBrowserClientHandlers<T> {
  fn clone(&self) -> Self {
    Self {
      frame_event_handler: self.frame_event_handler.clone(),
      view_event_handler: self.view_event_handler.clone(),
      ipc_handler: self.ipc_handler.clone(),
      on_page_load_handler: self.on_page_load_handler.clone(),
      document_title_changed_handler: self.document_title_changed_handler.clone(),
      navigation_handler: self.navigation_handler.clone(),
      resource_request_handler: self.resource_request_handler.clone(),
      browser_request_policy: self.browser_request_policy.clone(),
      session_request_policy: self.session_request_policy.clone(),
      new_window_handler: self.new_window_handler.clone(),
      download_handler: self.download_handler.clone(),
      console_message_handler: self.console_message_handler.clone(),
      permission_request_handler: self.permission_request_handler.clone(),
      web_content_process_terminate_handler: self.web_content_process_terminate_handler.clone(),
    }
  }
}

wrap_with_args! {
  wrap_client => TauriCefBrowserClientArgs;

  pub(crate) struct TauriCefBrowserClient<T: UserEvent> {
    pub(crate) context: RuntimeContext<T>,
    pub(crate) window_id: WindowId,
    pub(crate) webview_id: u32,
    pub(crate) label: String,
    initial_url: Option<String>,
    devtools_enabled: bool,
    zoom_hotkeys_enabled: bool,
    allowed_chrome_commands: Vec<crate::ChromeCommandGroup>,
    drag_drop_event_target: DragDropEventTarget,
    drag_drop_handler_enabled: bool,
    drag_drop_state: Arc<Mutex<DragDropState>>,
    pub(crate) frame_navigation_state: crate::FrameNavigationState,
    popup_family: Weak<crate::popup::PopupFamily>,
    opener: Option<crate::popup::PopupRequest>,
    render_handler: Option<cef::RenderHandler>,
    cursor_handler: Option<Arc<crate::CursorHandler>>,
    pub(crate) handlers: TauriCefBrowserClientHandlers<T>,
    sender: Sender<Message<T>>,
  }

  impl Client {
    fn render_handler(&self) -> Option<RenderHandler> {
      self.render_handler.clone()
    }

    fn frame_handler(&self) -> Option<FrameHandler> {
      self.handlers.frame_event_handler.as_ref().map(|handler| {
        frame::TauriCefFrameHandler::new(Some(handler.clone()))
      })
    }

    fn drag_handler(&self) -> Option<DragHandler> {
      self
        .drag_drop_handler_enabled
        .then(|| TauriCefDragHandler::new(self.drag_drop_state.clone()))
    }

    fn request_handler(&self) -> Option<RequestHandler> {
      Some(request_handler::WebRequestHandler::build(
        request_handler::WebRequestHandlerArgs {
          navigation_handler: self.handlers.navigation_handler.clone(),
          resource_request_handler: self.handlers.resource_request_handler.clone(),
          browser_request_policy: self.handlers.browser_request_policy.clone(),
          session_request_policy: self.handlers.session_request_policy.clone(),
          frame_event_handler: self.handlers.frame_event_handler.clone(),
          context: self.context.clone(),
          window_id: self.window_id,
          webview_id: self.webview_id,
          drag_drop_event_target: self.drag_drop_event_target,
          drag_drop_handler_enabled: self.drag_drop_handler_enabled,
          drag_drop_state: self.drag_drop_state.clone(),
          web_content_process_terminate_handler:
            self.handlers.web_content_process_terminate_handler.clone(),
          certificate_errors: self.context.certificate_errors,
        },
      ))
    }

    fn life_span_handler(&self) -> Option<LifeSpanHandler> {
      let context = self.context.clone();
      let window_id = self.window_id;
      let webview_id = self.webview_id;
      let label = self.label.clone();
      let devtools_enabled = self.devtools_enabled;
      let zoom_hotkeys_enabled = self.zoom_hotkeys_enabled;
      // A CEF-owned popup is a real Chrome window, not an app window, so it keeps the
      // opener's allowances rather than being locked down harder than its opener.
      let allowed_chrome_commands = self.allowed_chrome_commands.clone();
      let target = self.drag_drop_event_target;
      let navigation_handler = self.handlers.navigation_handler.clone();
      let resource_request_handler = self.handlers.resource_request_handler.clone();
      let new_window_handler = self.handlers.new_window_handler.clone();
      let session_request_policy = self.handlers.session_request_policy.clone();
      let download_handler = self.handlers.download_handler.clone();
      let permission_request_handler = self.handlers.permission_request_handler.clone();
      let family = self.popup_family.clone();
      let create_popup: Arc<life_span::PopupClientFactory> = Arc::new(move |opener, state| {
        let events = state.clone();
        TauriCefBrowserClient::build(TauriCefBrowserClientArgs {
          context: context.clone(),
          window_id,
          webview_id,
          label: label.clone(),
          initial_url: None,
          devtools_enabled,
          zoom_hotkeys_enabled,
          allowed_chrome_commands: allowed_chrome_commands.clone(),
          drag_drop_event_target: target,
          drag_drop_handler_enabled: false,
          drag_drop_state: Arc::default(),
          frame_navigation_state: state,
          popup_family: family.clone(),
          opener: Some(opener),
          render_handler: None,
          cursor_handler: None,
          handlers: TauriCefBrowserClientHandlers {
            // Only the internal navigation observer, never the opener's app
            // observer. A popup is a separate native browser that navigates
            // wherever its own content goes — an SSO or OAuth window is the
            // standing case — and every `FrameEvent` carries the full URL. An
            // app observes popups without their URLs through `Webview::popups`.
            frame_event_handler: Some(Arc::new(move |event| events.on_frame_event(&event))),
            view_event_handler: None,
            navigation_handler: navigation_handler.clone(),
            resource_request_handler: resource_request_handler.clone(),
            browser_request_policy: None,
            session_request_policy: session_request_policy.clone(),
            new_window_handler: new_window_handler.clone(),
            // CEF cancels every download of a client whose download handler is
            // NULL, so the popup keeps the opener's — as it did when it still
            // inherited the opener's client outright.
            download_handler: download_handler.clone(),
            // The opener's refusals carry over, its grants do not. A popup shows
            // content the opener navigated to — an SSO or OAuth window is the
            // standing case — and a `PermissionKind` names no origin, so an
            // `Allow` the app gave for its own content is no answer about that
            // other content; CEF's own prompt asks the user instead. A `Deny`
            // carries over so a refused permission cannot be obtained by opening
            // a popup.
            permission_request_handler: permission_request_handler.clone().map(|handler| {
              Arc::new(move |kind| match handler(kind) {
                tauri_runtime::webview::PermissionResponse::Allow => {
                  tauri_runtime::webview::PermissionResponse::Default
                }
                response => response,
              }) as Arc<PermissionRequestHandler>
            }),
            // A `ConsoleMessage` carries the source URL of whatever logged it, so
            // routing a popup's output to the opener's observer would report an
            // SSO or OAuth window's URLs to an observer registered for the app's
            // own content.
            console_message_handler: None,
            ipc_handler: None,
            on_page_load_handler: None,
            document_title_changed_handler: None,
            web_content_process_terminate_handler: None,
          },
          sender: context.sender.clone(),
        })
      });
      Some(TauriCefChildLifeSpanHandler::build(TauriCefChildLifeSpanHandlerArgs {
        sender: self.sender.clone(),
        window_id: self.window_id,
        webview_id: self.webview_id,
        context: self.context.clone(),
        new_window_handler: self.handlers.new_window_handler.clone(),
        initial_url: self.initial_url.clone(),
        frame_navigation_state: self.frame_navigation_state.clone(),
        popup_family: self.popup_family.clone(),
        opener: self.opener.clone(),
        create_popup,
      }))
    }

    fn load_handler(&self) -> Option<LoadHandler> {
      Some(TauriCefLoadHandler::new(
        self.handlers.on_page_load_handler.clone(),
        self.handlers.frame_event_handler.clone(),
        self.frame_navigation_state.clone(),
      ))
    }

    fn focus_handler(&self) -> Option<FocusHandler> {
      Some(focus::TauriCefFocusHandler::new(
        self.handlers.view_event_handler.clone(),
        self.frame_navigation_state.clone(),
      ))
    }

    fn display_handler(&self) -> Option<DisplayHandler> {
      Some(TauriCefDisplayHandler::build(TauriCefDisplayHandlerArgs {
        webview_id: self.webview_id,
        context: self.context.clone(),
        document_title_changed_handler: self.handlers.document_title_changed_handler.clone(),
        frame_event_handler: self.handlers.frame_event_handler.clone(),
        view_event_handler: self.handlers.view_event_handler.clone(),
        console_message_handler: self.handlers.console_message_handler.clone(),
        cursor_handler: self.cursor_handler.clone(),
        frame_navigation_state: self.frame_navigation_state.clone(),
      }))
    }

    fn download_handler(&self) -> Option<DownloadHandler> {
      self
        .handlers
        .download_handler
        .clone()
        .map(TauriCefDownloadHandler::new)
    }

    fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
      Some(TauriCefContextMenuHandler::new(self.devtools_enabled, self.label.clone()))
    }

    fn keyboard_handler(&self) -> Option<KeyboardHandler> {
      Some(TauriCefKeyboardHandler::new(self.devtools_enabled))
    }

    fn command_handler(&self) -> Option<CommandHandler> {
      Some(TauriCefCommandHandler::build(TauriCefCommandHandlerArgs {
        webview_id: self.webview_id,
        context: self.context.clone(),
        devtools_enabled: self.devtools_enabled,
        zoom_hotkeys_enabled: self.zoom_hotkeys_enabled,
        allowed_chrome_commands: self.allowed_chrome_commands.clone(),
        frame_navigation_state: self.frame_navigation_state.clone(),
      }))
    }

    fn permission_handler(&self) -> Option<PermissionHandler> {
      Some(TauriCefPermissionHandler::new(
        self.handlers.permission_request_handler.clone(),
      ))
    }

    fn on_native_observed_navigation(
      &self, browser: Option<&mut Browser>, issuance: u64, nonce: Option<&CefString>,
      committed: i32, error: i32, url: Option<&CefString>, document: Option<&CefString>,
      devtools_frame: Option<&CefString>, devtools_loader: Option<&CefString>,
      process: Option<&mut NativeRendererProcess>, lifetime: Option<&mut NativeObservedDocument>,
    ) {
      let Some(browser) = browser else { return };
      let state = &self.frame_navigation_state;
      if self.opener.is_some() || browser.is_popup() != 0
        || state.trusted_init_mode() != Some(crate::TrustedInitMode::Unprotected)
        || !state.has_browser_id(browser.identifier()) { return; }
      state.observed_navigation.complete(state.clone(), issuance,
        nonce.map(ToString::to_string).unwrap_or_default(), committed != 0, error,
        url.map(ToString::to_string).unwrap_or_default(),
        document.map(ToString::to_string).unwrap_or_default(),
        devtools_frame.map(ToString::to_string).unwrap_or_default(),
        devtools_loader.map(ToString::to_string).unwrap_or_default(),
        process.map(|value| value.clone()), lifetime.map(|value| value.clone()));
    }

    fn on_native_context_registered(
      &self, browser: Option<&mut Browser>, frame: Option<&mut Frame>,
      frame_id: Option<&CefString>, child_id: i32, document: Option<&CefString>,
      context: Option<&CefString>, issuance: u64, intent: Option<&CefString>,
      world: i32, document_url: Option<&CefString>,
      evaluation: Option<&mut NativeContextEvaluation>, channel: Option<&mut NativeContextChannel>,
      process: Option<&mut NativeRendererProcess>,
    ) -> i32 {
      let (Some(browser), Some(frame), Some(frame_id), Some(document), Some(context),
        Some(intent), Some(document_url), Some(evaluation), Some(channel)) =
        (browser, frame, frame_id, document, context, intent, document_url, evaluation, channel) else { return 0 };
      if self.opener.is_some() || browser.is_popup() != 0
        || !self.frame_navigation_state.has_browser_id(browser.identifier()) { return 0; }
      let Ok(document_url) = url::Url::parse(&document_url.to_string()) else { return 0 };
      let Some(process) = process.and_then(|process| crate::NativeRendererProcess::from_native(process)) else { return 0 };
      i32::from(ipc::on_native_context_registered(self, browser, frame, &frame_id.to_string(),
        crate::native_context::VerifiedNativeIdentity {
          renderer_child_id: child_id, document_token: document.to_string(),
          context_token: context.to_string(), world_id: world,
          registration: issuance,
        }, issuance, &intent.to_string(), crate::native_invoke::NativeContextEndpoints {
          channel: channel.clone(), evaluation: evaluation.clone(), document_url,
          renderer_process: process,
        }))
    }

    fn on_native_context_process_will_exit(
      &self, browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
      frame_id: Option<&CefString>, child_id: i32, document: Option<&CefString>,
      context: Option<&CefString>, world: i32, registration: u64,
    ) {
      let (Some(browser), Some(frame_id), Some(document), Some(context)) =
        (browser, frame_id, document, context) else { return };
      ipc::on_native_context_process_will_exit(self, browser.identifier(), &frame_id.to_string(),
        &crate::native_context::VerifiedNativeIdentity {
          renderer_child_id: child_id, document_token: document.to_string(),
          context_token: context.to_string(), world_id: world, registration,
        });
    }

    fn on_native_context_process_did_exit(&self) {
      ipc::on_native_context_did_revoke();
    }

    fn on_native_context_will_revoke(
      &self, browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
      frame_id: Option<&CefString>, child_id: i32, document: Option<&CefString>,
      context: Option<&CefString>, world: i32, registration: u64,
    ) {
      let (Some(browser), Some(frame_id), Some(document), Some(context)) =
        (browser, frame_id, document, context) else { return };
      ipc::on_native_context_will_revoke(self, browser.identifier(), &frame_id.to_string(),
        &crate::native_context::VerifiedNativeIdentity {
          renderer_child_id: child_id, document_token: document.to_string(),
          context_token: context.to_string(), world_id: world,
          registration,
        });
    }

    fn on_native_context_did_revoke(
      &self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
      _frame_id: Option<&CefString>, _child_id: i32, _document: Option<&CefString>,
      _context: Option<&CefString>, _world: i32, _registration: u64,
    ) {
      ipc::on_native_context_did_revoke();
    }

    fn on_native_context_revoked(
      &self, browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
      frame_id: Option<&CefString>, child_id: i32, document: Option<&CefString>,
      context: Option<&CefString>, world: i32, registration: u64,
    ) {
      let (Some(browser), Some(frame_id), Some(document), Some(context)) =
        (browser, frame_id, document, context) else { return };
      ipc::on_native_context_revoked(self, browser, &frame_id.to_string(),
        &crate::native_context::VerifiedNativeIdentity {
          renderer_child_id: child_id, document_token: document.to_string(),
          context_token: context.to_string(), world_id: world,
          registration,
        });
    }

    fn on_native_context_tauri_request(
      &self, browser: Option<&mut Browser>, frame_id: Option<&CefString>, child_id: i32,
      document: Option<&CefString>, context: Option<&CefString>, world: i32, registration: u64,
      cmd: Option<&CefString>, callback: u32, error: u32, raw: i32,
      invoke_key: Option<&CefString>, headers_json: Option<&CefString>, body: Option<&mut BinaryValue>,
    ) {
      let (Some(browser), Some(frame_id), Some(document), Some(context), Some(cmd), Some(invoke_key), Some(headers_json)) =
        (browser, frame_id, document, context, cmd, invoke_key, headers_json) else { return };
      let Some(body) = crate::native_invoke::binary_bytes(body.as_deref()) else { return };
      ipc::on_native_tauri_request(self, browser.identifier(), &frame_id.to_string(),
        &crate::native_context::VerifiedNativeIdentity {
          renderer_child_id: child_id, document_token: document.to_string(),
          context_token: context.to_string(), world_id: world,
          registration,
        }, cmd.to_string(), callback, error, raw != 0, invoke_key.to_string(), headers_json.to_string(), body);
    }

    fn on_native_context_sync_request(
      &self, browser: Option<&mut Browser>, frame_id: Option<&CefString>, child_id: i32,
      document: Option<&CefString>, context: Option<&CefString>, world: i32, registration: u64,
      channel: Option<&CefString>, body: Option<&mut BinaryValue>, reply: Option<&mut NativeContextSyncReply>,
    ) {
      let Some(reply) = reply else { return };
      let (Some(browser), Some(frame_id), Some(document), Some(context), Some(channel)) =
        (browser, frame_id, document, context, channel) else { reply.complete(0, None); return };
      let Some(body) = crate::native_invoke::binary_bytes(body.as_deref()) else { reply.complete(0, None); return };
      ipc::on_native_sync_request(self, browser.identifier(), &frame_id.to_string(),
        &crate::native_context::VerifiedNativeIdentity {
          renderer_child_id: child_id, document_token: document.to_string(),
          context_token: context.to_string(), world_id: world,
          registration,
        }, channel.to_string(), body, reply.clone());
    }

    fn on_native_context_first_non_empty_layout(
      &self, browser: Option<&mut Browser>, frame_id: Option<&CefString>, child_id: i32,
      document: Option<&CefString>, context: Option<&CefString>, world: i32, registration: u64,
    ) {
      let (Some(browser), Some(frame_id), Some(document), Some(context)) =
        (browser, frame_id, document, context) else { return };
      ipc::on_native_first_nonempty_layout(self, browser.identifier(), &frame_id.to_string(),
        &crate::native_context::VerifiedNativeIdentity {
          renderer_child_id: child_id, document_token: document.to_string(),
          context_token: context.to_string(), world_id: world,
          registration,
        });
    }

    fn on_native_context_document_ready(
      &self, browser: Option<&mut Browser>, frame_id: Option<&CefString>, child_id: i32,
      document: Option<&CefString>, context: Option<&CefString>, world: i32, registration: u64,
    ) {
      let (Some(browser), Some(frame_id), Some(document), Some(context)) =
        (browser, frame_id, document, context) else { return };
      let state = &self.frame_navigation_state;
      let identity = crate::native_context::VerifiedNativeIdentity {
        renderer_child_id: child_id, document_token: document.to_string(),
        context_token: context.to_string(), world_id: world,
        registration,
      };
      if let Some(origin) = state.native_contexts.verified_token(state, browser.identifier(), &frame_id.to_string(), &identity) {
        if !origin.mark_document_ready() { return; }
        state.native_reload.document_ready(&origin);
        if let Some(handler) = &self.handlers.frame_event_handler {
          handler(crate::FrameEvent { browser_id: browser.identifier(), frame_id: frame_id.to_string(), is_main: true,
            kind: crate::FrameEventKind::NativeDocumentReady { origin } });
        }
      }
    }

    fn on_process_message_received(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      source_process: ProcessId,
      message: Option<&mut ProcessMessage>,
    ) -> std::os::raw::c_int {
      ipc::record_renderer_identity(browser.as_deref(), frame.as_deref(), source_process, message.as_deref());
      // A CEF popup (including DevTools) never inherits the root IPC identity.
      if self.opener.is_some() || browser.as_ref().is_none_or(|browser| browser.is_popup() != 0 || !self.frame_navigation_state.has_browser_id(browser.identifier())) { return 0; }
      ipc::on_process_message_received(self, browser, frame, source_process, message)
    }
  }
}
