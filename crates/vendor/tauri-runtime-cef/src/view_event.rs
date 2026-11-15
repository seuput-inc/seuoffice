// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::Arc;

/// Native focus and HTML fullscreen notifications for one webview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
  Focus,
  /// The native child view lost focus or its containing application deactivated.
  Blur,
  /// A Chromium main-frame navigation start, before commit or failure.
  NavigationStarted(crate::NavigationStart),
  EnterHtmlFullScreen,
  LeaveHtmlFullScreen,
}

/// Synchronous observer called on the CEF UI thread; it must return promptly.
pub type ViewEventHandler = dyn Fn(crate::FrameNavigationState, ViewEvent) + Send + Sync + 'static;

pub(crate) fn emit_view_event(
  handler: &Option<Arc<ViewEventHandler>>,
  state: &crate::FrameNavigationState,
  browser_id: i32,
  is_popup: bool,
  event: ViewEvent,
) {
  if !is_popup && state.has_browser_id(browser_id) && let Some(handler) = handler {
    handler(state.clone(), event);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Mutex;

  #[test]
  fn reports_only_the_owned_non_popup_browser() {
    let state = crate::FrameNavigationState::new();
    state.on_frame_event(&crate::FrameEvent {
      browser_id: 7,
      frame_id: "main".into(),
      is_main: true,
      kind: crate::FrameEventKind::Created,
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let collected = events.clone();
    let handler: Option<Arc<ViewEventHandler>> = Some(Arc::new(move |_, event| collected.lock().unwrap().push(event)));
    emit_view_event(&handler, &state, 7, false, ViewEvent::Focus);
    emit_view_event(&handler, &state, 7, false, ViewEvent::Blur);
    emit_view_event(&handler, &state, 8, false, ViewEvent::Blur);
    emit_view_event(&handler, &state, 7, true, ViewEvent::Blur);
    emit_view_event(&handler, &state, 8, false, ViewEvent::Focus);
    emit_view_event(&handler, &state, 7, true, ViewEvent::Focus);
    emit_view_event(&handler, &state, 7, false, ViewEvent::EnterHtmlFullScreen);
    emit_view_event(&handler, &state, 7, false, ViewEvent::LeaveHtmlFullScreen);
    assert_eq!(*events.lock().unwrap(), vec![ViewEvent::Focus, ViewEvent::Blur, ViewEvent::EnterHtmlFullScreen, ViewEvent::LeaveHtmlFullScreen]);
  }

  #[test]
  fn late_blur_is_not_delivered_to_a_closed_browser_even_if_its_id_is_reused() {
    let old = crate::FrameNavigationState::new();
    assert!(old.bind_browser(7));
    let replacement = crate::FrameNavigationState::new();
    assert!(replacement.bind_browser(7));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collected = events.clone();
    let handler: Option<Arc<ViewEventHandler>> =
      Some(Arc::new(move |_, event| collected.lock().unwrap().push(event)));

    old.close();
    emit_view_event(&handler, &old, 7, false, ViewEvent::Blur);
    emit_view_event(&handler, &replacement, 7, false, ViewEvent::Focus);
    assert_eq!(*events.lock().unwrap(), vec![ViewEvent::Focus]);
  }

  #[test]
  fn focus_carries_original_browser_identity_across_document_changes() {
    let original = crate::FrameNavigationState::new();
    let replacement = crate::FrameNavigationState::new();
    assert!(original.bind_browser(7));
    assert!(replacement.bind_browser(7));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let output = captured.clone();
    let handler: Option<Arc<ViewEventHandler>> = Some(Arc::new(move |browser, _| {
      output.lock().unwrap().push(browser);
    }));
    emit_view_event(&handler, &original, 7, false, ViewEvent::Focus);
    original.on_protocol_navigation(false);
    emit_view_event(&handler, &original, 7, false, ViewEvent::Blur);
    emit_view_event(&handler, &replacement, 7, false, ViewEvent::Focus);
    let events = captured.lock().unwrap();
    assert!(events[0].is_same_browser(&original));
    assert!(!events[0].is_same_browser(&replacement));
    assert!(events[1].is_same_browser(&original));
    assert!(events[2].is_same_browser(&replacement));
  }
}
