// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

/// CEF's native requester identity for a custom-scheme request.
///
/// This value is attached to the HTTP request extensions by the runtime, not
/// taken from request headers or the protocol handler's webview label. Browserless
/// requests, including service-worker requests, have no browser or frame identity.
/// `browser_state`, `navigation`, and `document` are observations made when
/// CEF creates the scheme handler. They do not identify the V8 context that
/// issued the request: an older document's request can arrive after navigation
/// and observe the replacement document. Do not use them to authorize
/// document-scoped operations without independent context authentication.
#[derive(Clone, Debug)]
pub struct SchemeRequestProvenance {
  pub(crate) browser_id: Option<i32>,
  pub(crate) frame_id: Option<String>,
  pub(crate) frame_is_main: Option<bool>,
  pub(crate) browser_state: Option<crate::FrameNavigationState>,
  pub(crate) navigation: Option<crate::NativeNavigationToken>,
  pub(crate) document: Option<crate::NativeDocumentToken>,
  pub(crate) native_document_owner: Option<crate::NativeContextToken>,
}

impl SchemeRequestProvenance {
  pub fn browser_id(&self) -> Option<i32> { self.browser_id }
  pub fn frame_id(&self) -> Option<&str> { self.frame_id.as_deref() }
  pub fn frame_is_main(&self) -> Option<bool> { self.frame_is_main }
  pub fn browser_state(&self) -> Option<&crate::FrameNavigationState> { self.browser_state.as_ref() }
  pub fn navigation(&self) -> Option<&crate::NativeNavigationToken> { self.navigation.as_ref() }
  /// The document observed when the handler was created, not the request's
  /// authenticated originating document.
  pub fn document(&self) -> Option<&crate::NativeDocumentToken> { self.document.as_ref() }
  /// Exact native factory/request document captured before application IO.
  /// This does not prove a JavaScript issuing realm or authorize IPC.
  pub fn native_document_owner(&self) -> Option<&crate::NativeContextToken> {
    self.native_document_owner.as_ref()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn request_extensions_preserve_native_lifetime_and_browserless_identity() {
    let state = crate::FrameNavigationState::new();
    let provenance = SchemeRequestProvenance {
      browser_id: Some(7),
      frame_id: Some("frame".into()),
      frame_is_main: Some(true),
      browser_state: Some(state.clone()),
      navigation: None,
      document: None,
      native_document_owner: None,
    };
    let mut request = http::Request::new(Vec::<u8>::new());
    request.extensions_mut().insert(provenance);
    let (parts, body) = request.into_parts();
    let request = http::Request::from_parts(parts, body);
    let origin = request.extensions().get::<SchemeRequestProvenance>().unwrap();
    assert_eq!(origin.browser_id(), Some(7));
    assert_eq!(origin.frame_id(), Some("frame"));
    assert_eq!(origin.frame_is_main(), Some(true));
    assert!(origin.browser_state().unwrap().is_same_browser(&state));
    assert!(!origin.browser_state().unwrap().is_same_browser(&crate::FrameNavigationState::new()));
    assert!(origin.document().is_none());

    let browserless = SchemeRequestProvenance {
      browser_id: None, frame_id: None, frame_is_main: None, browser_state: None, document: None, navigation: None,
      native_document_owner: None,
    };
    assert!(browserless.browser_id().is_none());
    assert!(browserless.frame_id().is_none());
    assert!(browserless.frame_is_main().is_none());
    assert!(browserless.browser_state().is_none());
    assert!(browserless.document().is_none());
    assert!(browserless.navigation().is_none());
  }

  #[test]
  fn request_epoch_cannot_authorize_a_replaced_or_closed_browser() {
    let state = crate::FrameNavigationState::new();
    assert!(state.bind_browser(7));
    let request_epoch = state.navigation().unwrap();
    let provenance = SchemeRequestProvenance {
      browser_id: Some(7), frame_id: Some("main".into()), frame_is_main: Some(true),
      browser_state: Some(state.clone()), navigation: Some(request_epoch.clone()), document: None,
      native_document_owner: None,
    };
    assert_eq!(provenance.navigation(), state.navigation().as_ref());
    state.on_frame_event(&crate::FrameEvent {
      browser_id: 7,
      frame_id: "main".into(),
      is_main: true,
      kind: crate::FrameEventKind::NavigationStarted { url: url::Url::parse("https://example.test/next").unwrap() },
    });
    assert_ne!(provenance.navigation(), state.navigation().as_ref());
    assert!(!provenance.browser_state().unwrap().is_same_browser(&crate::FrameNavigationState::new()));
    state.close();
    assert!(state.navigation().is_none());
  }
}
