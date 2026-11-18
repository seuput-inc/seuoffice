// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#[cfg(target_os = "macos")]
mod native {
  use std::sync::{Arc, Mutex};

  use objc2::{
    MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::NSObjectProtocol,
  };
  use objc2_app_kit::{NSApp, NSApplicationActivationPolicy, NSView, NSWindow};
  use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

  define_class!(
    #[unsafe(super(NSView))]
    #[name = "TauriCefNativeFocusTestView"]
    #[thread_kind = MainThreadOnly]
    struct TestView;

    unsafe impl NSObjectProtocol for TestView {}

    impl TestView {
      #[unsafe(method(acceptsFirstResponder))]
      fn accepts_first_responder(&self) -> bool { true }
    }
  );

  fn view(mtm: MainThreadMarker) -> Retained<TestView> {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(80.0, 60.0));
    unsafe { msg_send![TestView::alloc(mtm), initWithFrame: frame] }
  }

  fn window(mtm: MainThreadMarker) -> Retained<NSWindow> {
    let rect = NSRect::new(NSPoint::new(80.0, 80.0), NSSize::new(240.0, 160.0));
    let window: Retained<NSWindow> = unsafe {
      msg_send![NSWindow::alloc(mtm), initWithContentRect: rect, styleMask: 1usize, backing: 2usize, defer: false]
    };
    unsafe { window.setReleasedWhenClosed(false); }
    window
  }

  pub fn run() {
    let mtm = MainThreadMarker::new().expect("AppKit test must run on the process main thread");
    tauri_runtime_cef::prepare_macos_application();
    let app = NSApp(mtm);
    assert!(app.setActivationPolicy(NSApplicationActivationPolicy::Regular));
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let first = window(mtm);
    let second = window(mtm);
    let host = view(mtm);
    let child = view(mtm);
    let other = view(mtm);
    first.contentView().expect("first content view").addSubview(&host);
    host.addSubview(&child);
    first.contentView().expect("first content view").addSubview(&other);
    first.makeKeyAndOrderFront(None);
    assert!(app.isActive() && first.isKeyWindow(), "native test requires an active GUI session");
    assert!(first.makeFirstResponder(Some(&child)));

    let state = crate::FrameNavigationState::new();
    assert!(state.bind_browser(7));
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let failures = Arc::new(Mutex::new(Vec::new()));
    let errors = failures.clone();
    let dispatch: Arc<crate::focus::Dispatch> = Arc::new(|task| { task(); true });
    let observer = crate::focus::NativeFocusObserver::install(
      &*host as *const TestView as usize,
      Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
      state, 7, dispatch.clone(), dispatch,
      Arc::new(move |error| errors.lock().unwrap().push(error)),
    ).expect("native AppKit focus observer");

    assert!(first.makeFirstResponder(Some(&host)));
    assert!(events.lock().unwrap().is_empty(), "movement within the observed view must not blur");
    assert!(first.makeFirstResponder(Some(&other)));
    assert_eq!(*events.lock().unwrap(), vec![crate::ViewEvent::Blur]);
    assert!(first.makeFirstResponder(Some(&child)));
    second.makeKeyAndOrderFront(None);
    assert!(second.isKeyWindow());
    assert_eq!(*events.lock().unwrap(), vec![crate::ViewEvent::Blur, crate::ViewEvent::Blur]);
    first.makeKeyAndOrderFront(None);
    assert!(first.isKeyWindow());
    assert!(first.makeFirstResponder(Some(&child)));
    second.contentView().expect("second content view").addSubview(&host);
    observer.refresh().expect("reparent refresh");
    assert_eq!(*events.lock().unwrap(), vec![crate::ViewEvent::Blur, crate::ViewEvent::Blur, crate::ViewEvent::Blur]);
    drop(observer);
    assert!(second.makeFirstResponder(Some(&host)));
    assert!(second.makeFirstResponder(None));
    assert_eq!(*events.lock().unwrap(), vec![crate::ViewEvent::Blur, crate::ViewEvent::Blur, crate::ViewEvent::Blur]);
    assert!(failures.lock().unwrap().is_empty());
    first.close();
    second.close();
  }
}

// The included platform implementation is real. This adapter models only its
// browser lifetime/event boundary; it is not a CEF or runtime integration test.
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct FrameNavigationState(Arc<std::sync::atomic::AtomicI32>);

#[cfg(target_os = "macos")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
impl FrameNavigationState {
  fn new() -> Self { Self(Arc::new(std::sync::atomic::AtomicI32::new(0))) }
  fn bind_browser(&self, id: i32) -> bool {
    self.0.compare_exchange(0, id, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire).is_ok()
  }
  fn has_browser_id(&self, id: i32) -> bool { self.0.load(std::sync::atomic::Ordering::Acquire) == id }
  fn close(&self) { self.0.store(0, std::sync::atomic::Ordering::Release); }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewEvent { Focus, Blur }

#[cfg(target_os = "macos")]
pub type ViewEventHandler = dyn Fn(ViewEvent) + Send + Sync + 'static;

#[cfg(target_os = "macos")]
mod view_event {
  use super::*;
  pub(crate) fn emit_view_event(
    handler: &Option<Arc<ViewEventHandler>>, state: &FrameNavigationState,
    browser_id: i32, popup: bool, event: ViewEvent,
  ) {
    if !popup && state.has_browser_id(browser_id) && let Some(handler) = handler { handler(event); }
  }
}

#[cfg(target_os = "macos")]
#[path = "../src/platform/macos/focus.rs"]
mod focus;

#[cfg(target_os = "macos")]
fn main() { native::run(); }

#[cfg(not(target_os = "macos"))]
fn main() {}
