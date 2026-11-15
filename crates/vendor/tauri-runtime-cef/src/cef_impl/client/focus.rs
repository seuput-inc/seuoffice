// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use cef::*;

wrap_focus_handler! {
  pub struct TauriCefFocusHandler {
    handler: Option<Arc<crate::ViewEventHandler>>,
    frame_navigation_state: crate::FrameNavigationState,
  }

  impl FocusHandler {
    fn on_got_focus(&self, browser: Option<&mut Browser>) {
      if let Some(browser) = browser {
        #[cfg(target_os = "linux")]
        if browser.host().is_some_and(|host| crate::platform::linux::recover_modal_focus(host.window_handle() as _)) {
          return;
        }
        crate::view_event::emit_view_event(
          &self.handler,
          &self.frame_navigation_state,
          browser.identifier(),
          browser.is_popup() != 0,
          crate::ViewEvent::Focus,
        );
      }
    }
  }
}
