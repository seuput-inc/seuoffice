// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use cef::*;
use crate::FrameNavigationState;

pub(crate) enum Action { Navigate(url::Url), Reload, Back, Forward }

pub(crate) fn dispatch(browser: &Browser, state: &FrameNavigationState, action: Action) {
  if state.trusted_init_mode() == Some(crate::TrustedInitMode::Unprotected) {
    match action {
      Action::Navigate(url) => { if let Some(frame) = browser.main_frame() { frame.load_url(Some(&CefString::from(url.as_str()))); } }
      Action::Reload => browser.reload(),
      Action::Back => browser.go_back(),
      Action::Forward => browser.go_forward(),
    }
    return;
  }
  let Some(host) = browser.host() else { return };
  let Some((issuance, intent)) = state.native_contexts.issue_navigation_intent(state.clone(), browser.identifier()) else {
    log::error!("approved native navigation intent could not be issued");
    return;
  };
  let native_intent = CefString::from(intent.as_str());
  match action {
    Action::Navigate(url) => {
      let mut callback = ApprovedLoad::new(state.clone(), intent);
      host.load_approved_main_frame_url(Some(&CefString::from(url.as_str())), issuance, Some(&native_intent), Some(&mut callback));
    }
    action => {
      let mut callback = ApprovedHistory::new(state.clone(), intent);
      match action {
        Action::Reload => host.reload_approved_main_frame(0, issuance, Some(&native_intent), Some(&mut callback)),
        Action::Back => host.go_back_approved_main_frame(issuance, Some(&native_intent), Some(&mut callback)),
        Action::Forward => host.go_forward_approved_main_frame(issuance, Some(&native_intent), Some(&mut callback)),
        Action::Navigate(_) => unreachable!(),
      }
    }
  }
}

fn cancel(state: FrameNavigationState, intent: String) {
  crate::native_evaluation::defer_completion(Box::new(move || {
    state.native_contexts.cancel_navigation_intent(&state, &intent);
  }));
}

wrap_approved_navigation_callback! {
  struct ApprovedLoad { state: FrameNavigationState, intent: String }
  impl ApprovedNavigationCallback {
    fn on_complete(&self, accepted: i32) {
      if accepted == 0 { cancel(self.state.clone(), self.intent.clone()); }
    }
  }
}
wrap_approved_history_navigation_callback! {
  struct ApprovedHistory { state: FrameNavigationState, intent: String }
  impl ApprovedHistoryNavigationCallback {
    fn on_complete(&self, outcome: ApprovedHistoryNavigationOutcome) {
      if outcome != sys::cef_approved_history_navigation_outcome_t::CEF_APPROVED_HISTORY_STARTED_CROSS_DOCUMENT.into() {
        cancel(self.state.clone(), self.intent.clone());
      }
    }
  }
}
