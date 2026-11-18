// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use objc2::{DefinedClass, MainThreadOnly, define_class, rc::{Retained, Weak}, runtime::{NSObject, NSObjectProtocol}, sel};
use objc2_app_kit::{NSWindow, NSWindowWillCloseNotification};
use objc2_foundation::{MainThreadMarker, NSNotification, NSNotificationCenter};

pub(crate) struct PrecloseIvars {
  window: Weak<NSWindow>,
  callback: Box<dyn Fn(Option<&NSWindow>)>,
}
define_class!(
  #[unsafe(super(NSObject))]
  #[name = "TauriCefNativeWindowPreclose"]
  #[ivars = PrecloseIvars]
  #[thread_kind = MainThreadOnly]
  pub(crate) struct PrecloseObserver;

  unsafe impl NSObjectProtocol for PrecloseObserver {}

  impl PrecloseObserver {
    #[unsafe(method(windowWillClose:))]
    fn window_will_close(&self, _: &NSNotification) {
      let window = self.ivars().window.load();
      (self.ivars().callback)(window.as_deref());
    }
  }
);
impl PrecloseObserver {
  /// The notification is delivered synchronously while its NSWindow remains
  /// valid. Retaining this observer does not retain that window.
  pub(crate) fn observe(window: &NSWindow, callback: impl Fn(Option<&NSWindow>) + 'static) -> Retained<Self> {
    let observer = Self::alloc(MainThreadMarker::from(window)).set_ivars(PrecloseIvars {
      window: Weak::new(window), callback: Box::new(callback),
    });
    let observer: Retained<Self> = unsafe { objc2::msg_send![super(observer), init] };
    unsafe {
      NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
        &observer, sel!(windowWillClose:), Some(NSWindowWillCloseNotification), Some(window),
      );
    }
    observer
  }
}
