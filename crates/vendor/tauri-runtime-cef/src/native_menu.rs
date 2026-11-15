// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::cell::RefCell;
use cef::*;
use crate::{FrameNavigationState, NativeContextToken};

#[derive(Clone)]
struct BrowserRecord { browser: Browser, state: FrameNavigationState }
thread_local! { static BROWSERS: RefCell<Vec<BrowserRecord>> = const { RefCell::new(Vec::new()) }; }

/// Captured synchronously in the real native menu callback before posting core.
/// It is not constructible from a renderer message, label, or menu-build ViewId.
#[derive(Clone)]
pub struct NativeMenuActivation { origin: NativeContextToken }
impl NativeMenuActivation {
  pub fn origin(&self) -> &NativeContextToken { &self.origin }
  pub fn is_live(&self) -> bool { self.origin.is_verified() }
}

pub(crate) fn created(browser: &Browser, state: &FrameNavigationState) {
  if browser.is_popup() != 0 { return; }
  BROWSERS.with(|records| {
    let mut records = records.borrow_mut();
    if records.iter().any(|record| record.state.is_same_browser(state)) { return; }
    records.push(BrowserRecord { browser: browser.clone(), state: state.clone() });
  });
}
pub(crate) fn closed(browser_id: i32, state: &FrameNavigationState) {
  BROWSERS.with(|records| records.borrow_mut().retain(|record|
    record.browser.identifier() != browser_id || !record.state.is_same_browser(state)));
}

/// The application calls this only from its actual OS menu activation callback.
/// Fresh native focus is sampled here; no event/focus cache or deferred lookup.
pub fn capture_native_menu_activation() -> Result<NativeMenuActivation, String> {
  if currently_on(sys::cef_thread_id_t::TID_UI.into()) != 1 {
    return Err("Native menu activation requires the native UI callback thread.".into());
  }
  BROWSERS.with(|records| {
    let records = records.try_borrow().map_err(|_| "Native menu browser registry is reentrant.".to_string())?.clone();
    let mut selected = None;
    for record in records.iter() {
      let id = record.browser.identifier();
      if !record.state.has_browser_id(id) || record.browser.is_valid() == 0 { continue; }
      let Some(host) = record.browser.host() else { continue };
      if host.is_window_rendering_disabled() != 0 || !native_focused(&host)? { continue; }
      let origin = record.state.native_contexts.menu_context(&record.state, id)
        .ok_or_else(|| "The focused native browser has no authenticated live context.".to_string())?;
      if selected.is_some() { return Err("Native menu activation matched multiple browsers.".into()); }
      selected = Some(NativeMenuActivation { origin });
    }
    selected.ok_or_else(|| "Native menu activation has no focused browser target.".into())
  })
}

#[cfg(windows)]
fn native_focused(host: &BrowserHost) -> Result<bool, String> {
  use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::{GA_ROOT, GUITHREADINFO, GetAncestor, GetForegroundWindow, GetGUIThreadInfo, IsChild, IsWindow}};
  let view = HWND(host.window_handle().0);
  if view.0.is_null() || !unsafe { IsWindow(Some(view)).as_bool() } { return Ok(false); }
  let active = unsafe { GetForegroundWindow() };
  if active.0.is_null() || unsafe { GetAncestor(view, GA_ROOT) } != active { return Ok(false); }
  let mut info = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
  unsafe { GetGUIThreadInfo(0, &mut info) }.map_err(|error| error.to_string())?;
  Ok(!info.hwndFocus.0.is_null() && (info.hwndFocus == view || unsafe { IsChild(view, info.hwndFocus).as_bool() }))
}

#[cfg(target_os = "macos")]
fn native_focused(host: &BrowserHost) -> Result<bool, String> {
  use objc2::{ClassType, MainThreadOnly, rc::Retained};
  use objc2_app_kit::{NSApp, NSView};
  let main = objc2::MainThreadMarker::new().ok_or_else(|| "Native menu focus requires AppKit main thread.".to_string())?;
  // The browser record retains the exact native host through close acknowledgement.
  let Some(view) = (unsafe { Retained::retain(host.window_handle().cast::<NSView>()) }) else { return Ok(false) };
  let Some(window) = view.window() else { return Ok(false) };
  if !NSApp(main).isActive() || !window.isKeyWindow() { return Ok(false); }
  let Some(responder) = window.firstResponder() else { return Ok(false) };
  if !responder.isKindOfClass(NSView::class()) { return Ok(false); }
  let responder = unsafe { &*(&*responder as *const _ as *const NSView) };
  Ok(std::ptr::eq(responder, &*view) || responder.isDescendantOf(&view))
}

#[cfg(target_os = "linux")]
fn native_focused(host: &BrowserHost) -> Result<bool, String> {
  use std::sync::LazyLock;
  use x11_dl::xlib;
  static XLIB: LazyLock<Option<xlib::Xlib>> = LazyLock::new(|| xlib::Xlib::open().ok());
  let xlib = XLIB.as_ref().ok_or_else(|| "Native menu X11 observation is unavailable.".to_string())?;
  let display = cef::get_xdisplay() as *mut xlib::Display;
  if display.is_null() { return Err("Native menu has no CEF X11 display.".into()); }
  let view = host.window_handle();
  if view == 0 { return Ok(false); }
  // CEF is a foreign X11 child, not a GTK focus widget. Observe actual server
  // input focus instead of GTK's cached focus widget or last focused view.
  let mut focus = 0;
  let mut revert = 0;
  unsafe { (xlib.XGetInputFocus)(display, &mut focus, &mut revert) };
  if focus == 0 || focus == 1 { return Ok(false); }
  for _ in 0..128 {
    if focus == view { return Ok(true); }
    let mut root = 0;
    let mut parent = 0;
    let mut children = std::ptr::null_mut();
    let mut count = 0;
    let ok = unsafe { (xlib.XQueryTree)(display, focus, &mut root, &mut parent, &mut children, &mut count) };
    if !children.is_null() { unsafe { (xlib.XFree)(children.cast()) }; }
    if ok == 0 || parent == 0 || parent == focus || focus == root { return Ok(false); }
    focus = parent;
  }
  Err("Native menu focused-window ancestry exceeded its bound.".into())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn native_focused(_: &BrowserHost) -> Result<bool, String> {
  Err("Native menu focus observation is unavailable on this platform.".into())
}
