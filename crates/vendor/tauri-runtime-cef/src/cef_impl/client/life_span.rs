// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Weak, mpsc::Sender};

use cef::*;
use tauri_runtime::{
  UserEvent,
  dpi::{LogicalPosition, LogicalSize},
  window::WindowId,
};

use crate::{
  macros::wrap_with_args,
  runtime::{CefRuntime, Message, NewWindowOpener, RuntimeContext},
};

pub(super) type PopupClientFactory =
  dyn Fn(crate::popup::PopupRequest, crate::FrameNavigationState) -> Client;

#[cfg(test)]
mod tests {
  use super::*;
  use tauri_runtime::webview::NewWindowFeatures;

  #[test]
  fn popup_source_observation_is_available_without_dispatch_and_redacted_from_debug() {
    let features =
      NewWindowFeatures::<(), CefRuntime<()>>::new(None, None, NewWindowOpener::new(None));
    assert!(features.opener().source_url().is_none());
    assert!(format!("{features:?}").contains("source_url_observed: false"));

    let source = url::Url::parse("https://example.com/private?token=fixture-secret").unwrap();
    let features = NewWindowFeatures::<(), CefRuntime<()>>::new(
      None,
      None,
      NewWindowOpener::new(Some(source.clone())),
    );
    assert_eq!(features.opener().source_url(), Some(&source));
    let debug = format!("{features:?}");
    assert!(debug.contains("source_url_observed: true"));
    assert!(!debug.contains("private"));
    assert!(!debug.contains("fixture-secret"));
  }
}

// There is some race condition on CEF that causes the app loading to fail
// when there is a network service crash:
// "[85296:47750637:0127/131203.017395:ERROR:content/browser/network_service_instance_impl.cc:610] Network service crashed or was terminated, restarting service."
// We check the app URL for a while until it actually loads the initial URL.
pub(crate) fn check_and_reload_if_blank(browser: cef::Browser, initial_url: String) {
  if initial_url == "about:blank" {
    return;
  }

  std::thread::spawn(move || {
    std::thread::sleep(std::time::Duration::from_secs(1));

    let start_time = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(5);
    let check_interval = std::time::Duration::from_millis(100);

    while start_time.elapsed() < timeout {
      if let Some(frame) = browser.main_frame() {
        let url = frame.url();
        let current_url = cef::CefString::from(&url).to_string();
        if current_url.is_empty() || current_url == "about:blank" {
          frame.load_url(Some(&cef::CefString::from(initial_url.as_str())));
          // Continue checking in case it loads about:blank again.
        } else {
          return;
        }
      }
      std::thread::sleep(check_interval);
    }
  });
}

wrap_with_args! {
  wrap_life_span_handler => TauriCefChildLifeSpanHandlerArgs;

  pub struct TauriCefChildLifeSpanHandler<T: UserEvent> {
    sender: Sender<Message<T>>,
    window_id: WindowId,
    webview_id: u32,
    context: RuntimeContext<T>,
    new_window_handler: Option<Arc<tauri_runtime::webview::NewWindowHandler<T, CefRuntime<T>>>>,
    initial_url: Option<String>,
    frame_navigation_state: crate::FrameNavigationState,
    popup_family: Weak<crate::popup::PopupFamily>,
    opener: Option<crate::popup::PopupRequest>,
    create_popup: Arc<PopupClientFactory>,
  }

  impl LifeSpanHandler {
    fn on_after_created(&self, browser: Option<&mut Browser>) {
      if let Some(browser) = browser.as_deref() {
        if !self.frame_navigation_state.bind_browser(browser.identifier()) {
          if let Some(host) = browser.host() { host.close_browser(1); }
          return;
        }
        crate::renderer_process::created(browser.identifier());
        crate::native_menu::created(browser, &self.frame_navigation_state);
      }
      if let Some(browser) = browser.as_deref().filter(|browser| browser.is_popup() == 0)
        && let Some(host) = browser.host()
      {
        if self.context.browser_lifetimes.created(self.webview_id, browser.identifier(), self.frame_navigation_state.clone(), host)
          && let Some(host) = self.context.browser_lifetimes.closing_host(self.webview_id, browser.identifier(), &self.frame_navigation_state) {
          host.close_browser(1);
        }
      }
      if let (Some(browser), Some(opener)) = (browser.as_deref(), self.opener.as_ref()) {
        if let Some(family) = self.popup_family.upgrade() {
          let _ = self.sender.send(Message::PopupCreated(opener.clone(), browser.identifier(), family.clone()));
          self.context.wake_event_loop();
          family.created(browser, opener, &self.frame_navigation_state);
        } else if let Some(host) = browser.host() {
          host.close_browser(1);
        }
        return;
      }
      if let Some(browser) = browser
        && browser.is_popup() == 0
        && let Some(initial_url) = &self.initial_url
      {
        check_and_reload_if_blank(browser.clone(), initial_url.clone());
      }
    }

    fn on_before_popup(
      &self,
      browser: Option<&mut Browser>,
      _frame: Option<&mut Frame>,
      popup_id: std::os::raw::c_int,
      target_url: Option<&CefString>,
      _target_frame_name: Option<&CefString>,
      _target_disposition: WindowOpenDisposition,
      _user_gesture: std::os::raw::c_int,
      popup_features: Option<&PopupFeatures>,
      window_info: Option<&mut WindowInfo>,
      client: Option<&mut Option<Client>>,
      _settings: Option<&mut BrowserSettings>,
      _extra_info: Option<&mut Option<DictionaryValue>>,
      _no_javascript_access: Option<&mut i32>,
    ) -> std::os::raw::c_int {
      if let Some(window_info) = window_info {
        window_info.windowless_rendering_enabled = 0;
        window_info.shared_texture_enabled = 0;
        window_info.external_begin_frame_enabled = 0;
      }
      let url_str = target_url.map(ToString::to_string).unwrap_or_default();
      let response = if let Some(handler) = &self.new_window_handler {
        let Ok(url) = url::Url::parse(&url_str) else { return 1; };
        // window.open features are CSS pixels, which map to logical units.
        let size = popup_features.and_then(|features| {
          (features.width_set != 0 && features.height_set != 0)
            .then(|| LogicalSize::new(features.width as f64, features.height as f64))
        });
        let position = popup_features.and_then(|features| {
          (features.x_set != 0 && features.y_set != 0)
            .then(|| LogicalPosition::new(features.x as f64, features.y as f64))
        });
        let native_origin = popup_features.filter(|features| features.native_context_authenticated == 1)
          .zip(browser.as_deref()).and_then(|(features, browser)| {
            let identity = crate::native_context::VerifiedNativeIdentity {
              renderer_child_id: features.native_renderer_child_id,
              world_id: features.native_world_id,
              document_token: format!("{:016X}{:016X}", features.native_document_high, features.native_document_low),
              context_token: format!("{:016X}{:016X}", features.native_context_high, features.native_context_low),
              registration: features.native_registration,
            };
            self.context.native_contexts.verified_popup_token(&self.frame_navigation_state, browser.identifier(), &identity)
          });
        let source_url = native_origin.as_ref().and_then(|origin| origin.endpoints())
          .map(|endpoints| endpoints.document_url);
        let mut opener = NewWindowOpener::new(source_url);
        if let Some(origin) = native_origin { opener = opener.with_native_origin(origin); }
        handler(url, tauri_runtime::webview::NewWindowFeatures::new(size, position, opener))
      } else { tauri_runtime::webview::NewWindowResponse::Allow };
      match response {
        tauri_runtime::webview::NewWindowResponse::Allow => {
          let (Some(browser), Some(client), Some(family)) =
            (browser, client, self.popup_family.upgrade()) else { return 1; };
          let Some(request) = family.reserve(&self.frame_navigation_state, browser.identifier(), popup_id) else { return 1; };
          if self.sender.send(Message::PopupPending(request.clone(), family.clone())).is_err() {
            family.abort(&self.frame_navigation_state, popup_id);
            return 1;
          }
          self.context.wake_event_loop();
          // Keep CEF's popup creation and JavaScript opener relationship. Only
          // its client changes: root IPC, load/title callbacks and close handling
          // cannot be inherited by a different native browser lifetime.
          *client = Some((self.create_popup)(request, crate::FrameNavigationState::new()));
          0
        },
        tauri_runtime::webview::NewWindowResponse::Create { window_id } => {
          // CEF cannot transplant a popup's contents into an existing
          // browser, so cancel the popup and navigate the designated
          // window's first webview to the URL instead — the closest
          // equivalent of wry hosting the popup in that window's webview.
          // Note `window.opener` is not linked to the new document.
          let _ = self.context.send_message(Message::NavigateFirstWebview {
            window_id,
            url: url_str,
          });
          1
        }
        tauri_runtime::webview::NewWindowResponse::Deny => 1,
      }
    }

    fn on_before_popup_aborted(&self, browser: Option<&mut Browser>, popup_id: std::os::raw::c_int) {
      let Some(browser) = browser else { return; };
      if !self.frame_navigation_state.has_browser_id(browser.identifier()) { return; }
      if let Some(family) = self.popup_family.upgrade()
        && let Some(request) = family.abort(&self.frame_navigation_state, popup_id) {
        let _ = self.sender.send(Message::PopupAborted(request));
        self.context.wake_event_loop();
      }
    }

    /// Take over the browser close so it does not take the window down with it.
    ///
    /// Returning 0 runs CEF's default, which sends the standard close
    /// notification to the browser's *top-level parent window* (`performClose:`
    /// on macOS, `WM_CLOSE` on Windows). Every Tauri webview is a child browser
    /// parented to a shared window, so that default turns "close this webview"
    /// into "close the window and every sibling webview" — and with the last
    /// window gone, the app exits.
    ///
    /// Returning 1 leaves the parent window alone and makes us responsible for
    /// completing the close, which means destroying this browser's own child
    /// view/window on the event loop. That destruction is what drives CEF's
    /// `WindowDestroyed` -> `on_before_close` sequence.
    ///
    /// On Linux the default only closes the browser's own X11 child window (and
    /// calls `WindowDestroyed` itself), so the default is already correct there.
    ///
    /// An offscreen browser has no native window or view of its own, so there is
    /// nothing to destroy and no top-level window the default could close: CEF
    /// completes the close itself.
    fn do_close(&self, browser: Option<&mut Browser>) -> std::os::raw::c_int {
      let Some(browser) = browser.filter(|browser| browser.is_popup() == 0) else {
        return 0;
      };
      if browser.host().is_some_and(|host| host.is_window_rendering_disabled() != 0)
      {
        return 0;
      }
      #[cfg(any(target_os = "macos", windows))]
      {
        if !self.context.browser_lifetimes.closing(self.webview_id, browser.identifier(), &self.frame_navigation_state) {
          return 0;
        }
        let _ = self
          .sender
          .send(Message::DestroyWebviewHostWindow(self.webview_id, browser.identifier(), self.frame_navigation_state.clone()));
        self.context.wake_event_loop();
        return 1;
      }

      #[cfg(not(any(target_os = "macos", windows)))]
      0
    }

    fn on_before_close(&self, browser: Option<&mut Browser>) {
      let Some(browser) = browser else { return; };
      crate::renderer_process::closed(browser.identifier());
      crate::native_menu::closed(browser.identifier(), &self.frame_navigation_state);
      if browser.is_popup() != 0 {
        self.frame_navigation_state.invalidate_browser_lifetime();
        self.frame_navigation_state.browser_close.complete(crate::NativeBrowserCloseOutcome::Closed);
        if let Some(family) = self.popup_family.upgrade() {
          family.closed(&self.frame_navigation_state, browser.identifier());
        }
        if self.opener.is_some() {
          let _ = self.sender.send(Message::PopupClosed(browser.identifier()));
          self.context.wake_event_loop();
        }
        return;
      }
      if !self.context.browser_lifetimes.closed(self.webview_id, browser.identifier(), &self.frame_navigation_state) {
        return;
      }
      if let Some(family) = self.popup_family.upgrade() {
        family.closed(&self.frame_navigation_state, browser.identifier());
      }
      let _ = self
        .sender
        .send(Message::BrowserClosed(self.window_id, self.webview_id, browser.identifier(), self.frame_navigation_state.clone()));
      self.context.wake_event_loop();
    }
  }
}
