// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{cell::{Cell, RefCell}, ffi::CString, sync::{Arc, Weak as SyncWeak, atomic::{AtomicBool, AtomicU64, Ordering}}};

use objc2::{
  ClassType, MainThreadOnly, define_class, ffi, msg_send,
  rc::{Retained, Weak},
  runtime::{AnyClass, AnyObject, Bool, Imp, NSObject, NSObjectProtocol, Sel},
  sel,
};
use objc2_app_kit::{
  NSApp, NSApplicationDidBecomeActiveNotification, NSApplicationDidResignActiveNotification,
  NSView, NSWindow, NSWindowDidBecomeKeyNotification, NSWindowDidResignKeyNotification,
};
use objc2_foundation::{MainThreadMarker, NSNotification, NSNotificationCenter};

use crate::{FrameNavigationState, ViewEvent, ViewEventHandler};

pub(crate) type Dispatch = dyn Fn(Box<dyn FnOnce() + Send>) -> bool + Send + Sync + 'static;
pub(crate) type Failure = dyn Fn(String) + Send + Sync + 'static;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Record {
  id: u64,
  view: Weak<NSView>,
  alive: Arc<AtomicBool>,
  focused: bool,
  callback: SyncWeak<Callback>,
}

struct Callback {
  handler: Option<Arc<ViewEventHandler>>,
  browser: FrameNavigationState,
  browser_id: i32,
  dispatch: Arc<Dispatch>,
  failure: Arc<Failure>,
}

struct WindowHook {
  window: Weak<NSWindow>,
  subclass: usize,
  original_class: usize,
}

#[derive(Clone, Copy)]
struct ClassHook {
  subclass: usize,
  original_class: usize,
  original_imp: Imp,
}

#[derive(Default)]
struct State {
  records: Vec<Record>,
  windows: Vec<WindowHook>,
  classes: Vec<ClassHook>,
  scratch_windows: Vec<Retained<NSWindow>>,
  notifications: Option<Retained<FocusNotifications>>,
}

thread_local! {
  static STATE: RefCell<State> = RefCell::new(State::default());
  static TRANSITIONS: Cell<usize> = const { Cell::new(0) };
}

define_class!(
  #[unsafe(super(NSObject))]
  #[name = "TauriCefFocusNotifications"]
  #[thread_kind = MainThreadOnly]
  struct FocusNotifications;

  unsafe impl NSObjectProtocol for FocusNotifications {}

  impl FocusNotifications {
    #[unsafe(method(focusChanged:))]
    fn focus_changed(&self, _: &NSNotification) {
      if TRANSITIONS.with(|depth| depth.get() == 0) {
        if let Err(error) = refresh_all() { report_failure(error); }
      }
    }
  }
);

fn notifications(mtm: MainThreadMarker) -> Retained<FocusNotifications> {
  let observer = FocusNotifications::alloc(mtm);
  let observer: Retained<FocusNotifications> = unsafe { msg_send![super(observer), init] };
  let center = NSNotificationCenter::defaultCenter();
  for name in unsafe {
    [NSWindowDidBecomeKeyNotification, NSWindowDidResignKeyNotification,
      NSApplicationDidBecomeActiveNotification, NSApplicationDidResignActiveNotification]
  } {
    unsafe { center.addObserver_selector_name_object(&observer, sel!(focusChanged:), Some(name), None); }
  }
  observer
}

fn clear_notifications(state: &mut State) {
  if let Some(observer) = state.notifications.take() {
    unsafe { NSNotificationCenter::defaultCenter().removeObserver(&observer); }
  }
}

unsafe extern "C-unwind" fn make_first_responder(
  window: *mut AnyObject, selector: Sel, responder: *mut AnyObject,
) -> Bool {
  let mut class = unsafe { ffi::object_getClass(window) };
  let original = STATE.with(|state| {
    let state = state.borrow();
    while !class.is_null() {
      if let Some(hook) = state.classes.iter().find(|hook| hook.subclass == class as usize) {
        return Some(hook.original_imp);
      }
      class = unsafe { ffi::class_getSuperclass(class) };
    }
    None
  });
  let Some(original) = original else {
    log::error!("native focus subclass lost its original implementation");
    return Bool::NO;
  };
  let original: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool =
    unsafe { std::mem::transmute(original) };
  TRANSITIONS.with(|depth| depth.set(depth.get() + 1));
  let result = unsafe { original(window, selector, responder) };
  let outermost = TRANSITIONS.with(|depth| {
    depth.set(depth.get() - 1);
    depth.get() == 0
  });
  if outermost {
    if let Err(error) = refresh_all() { report_failure(error); }
  }
  result
}

fn hook_window(state: &mut State, window: &NSWindow) -> Result<(), String> {
  let object = window as *const NSWindow as *mut AnyObject;
  let tracked = state.windows.iter().position(|hook| hook.window.load().is_some_and(|w| std::ptr::eq(&*w, window)));
  let class = unsafe { ffi::object_getClass(object) };
  if class.is_null() { return Err("native window has no Objective-C class".into()); }
  let mut ancestor = class;
  while !ancestor.is_null() {
    if let Some(hook) = state.classes.iter().find(|hook| hook.subclass == ancestor as usize) {
      if tracked.is_none() {
        state.windows.push(WindowHook {
          window: Weak::new(window), subclass: hook.subclass, original_class: hook.original_class,
        });
      }
      return Ok(());
    }
    ancestor = unsafe { ffi::class_getSuperclass(ancestor) };
  }
  let hook = register_class(state, class)?;
  unsafe { ffi::object_setClass(object, hook.subclass as *const AnyClass); }
  let tracked_hook = WindowHook {
    window: Weak::new(window), subclass: hook.subclass, original_class: hook.original_class,
  };
  if let Some(index) = tracked { state.windows[index] = tracked_hook; }
  else { state.windows.push(tracked_hook); }
  Ok(())
}

fn register_class(state: &mut State, class: *const AnyClass) -> Result<ClassHook, String> {
  if let Some(hook) = state.classes.iter().find(|hook| hook.original_class == class as usize) {
    return Ok(*hook);
  }
  let method = unsafe { ffi::class_getInstanceMethod(class, sel!(makeFirstResponder:)) };
  if method.is_null() { return Err("native window cannot observe responder changes".into()); }
  let original_imp = unsafe { ffi::method_getImplementation(method) }
    .ok_or_else(|| "native responder method has no implementation".to_string())?;
  let name = CString::new(format!("TauriCefFocusWindow_{:x}", class as usize)).unwrap();
  let subclass = unsafe { ffi::objc_allocateClassPair(class, name.as_ptr(), 0) };
  if subclass.is_null() { return Err("could not create native focus window subclass".into()); }
  let implementation: Imp = unsafe { std::mem::transmute(make_first_responder as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool) };
  let added = unsafe { ffi::class_addMethod(subclass, sel!(makeFirstResponder:), implementation, ffi::method_getTypeEncoding(method)) };
  if !added.as_bool() {
    unsafe { ffi::objc_disposeClassPair(subclass); }
    return Err("could not install native responder observer".into());
  }
  unsafe { ffi::objc_registerClassPair(subclass); }
  let hook = ClassHook { subclass: subclass as usize, original_class: class as usize, original_imp };
  state.classes.push(hook);
  Ok(hook)
}

unsafe fn restore_if_owned(object: *mut AnyObject, subclass: usize, original: usize) {
  if unsafe { ffi::object_getClass(object) } as usize == subclass {
    unsafe { ffi::object_setClass(object, original as *const AnyClass); }
  }
}

fn reconcile_windows(state: &mut State) -> Result<(), String> {
  let mut windows = std::mem::take(&mut state.scratch_windows);
  windows.extend(state.records.iter().filter_map(|record| record.view.load()?.window()));
  let result = (|| {
    for window in &windows { hook_window(state, window)?; }
    state.windows.retain(|hook| {
      let Some(window) = hook.window.load() else { return false; };
      if windows.iter().any(|candidate| std::ptr::eq(&**candidate, &*window)) { return true; }
      let object = &*window as *const NSWindow as *mut AnyObject;
      unsafe { restore_if_owned(object, hook.subclass, hook.original_class); }
      false
    });
    Ok(())
  })();
  windows.clear();
  state.scratch_windows = windows;
  result
}

fn focused(record: &Record, mtm: MainThreadMarker) -> bool {
  let Some(view) = record.view.load() else { return false; };
  let Some(window) = view.window() else { return false; };
  if !NSApp(mtm).isActive() || !window.isKeyWindow() { return false; }
  let Some(responder) = window.firstResponder() else { return false; };
  if !responder.isKindOfClass(NSView::class()) { return false; }
  let responder = unsafe { &*(&*responder as *const _ as *const NSView) };
  std::ptr::eq(responder, &*view) || responder.isDescendantOf(&view)
}

fn refresh_all() -> Result<(), String> {
  let mtm = MainThreadMarker::new().ok_or_else(|| "native focus refresh requires the main thread".to_string())?;
  let events = STATE.with(|state| {
    let mut state = state.borrow_mut();
    reconcile_windows(&mut state)?;
    let mut events = Vec::new();
    for record in &mut state.records {
      if !record.alive.load(Ordering::Acquire) { continue; }
      let now = focused(record, mtm);
      if record.focused && !now {
        events.push((record.alive.clone(), record.callback.clone()));
      }
      record.focused = now;
    }
    Ok::<_, String>(events)
  })?;
  deliver(events);
  Ok(())
}

fn deliver(events: Vec<(Arc<AtomicBool>, SyncWeak<Callback>)>) {
  for (alive, callback) in events {
    let Some(target) = callback.upgrade() else { continue };
    let dispatch = target.dispatch.clone();
    drop(target);
    if !(dispatch)(Box::new(move || {
      if alive.load(Ordering::Acquire) && let Some(callback) = callback.upgrade() {
        crate::view_event::emit_view_event(&callback.handler, &callback.browser, callback.browser_id, false, ViewEvent::Blur);
      }
    })) {
      log::error!("failed to dispatch native webview blur");
    }
  }
}

fn report_failure(error: String) {
  let callbacks = STATE.with(|state| {
    state.borrow().records.iter()
      .filter(|record| record.alive.load(Ordering::Acquire))
      .filter_map(|record| record.callback.upgrade())
      .map(|callback| (callback.dispatch.clone(), callback.failure.clone()))
      .collect::<Vec<_>>()
  });
  for (dispatch, failure) in callbacks {
    let error = error.clone();
    if !(dispatch)(Box::new(move || failure(error))) {
      log::error!("failed to dispatch native focus observer failure");
    }
  }
}

pub(crate) struct NativeFocusObserver {
  id: u64,
  alive: Arc<AtomicBool>,
  _callback: Arc<Callback>,
  cleanup_dispatch: Arc<Dispatch>,
}

impl NativeFocusObserver {
  pub(crate) fn install(
    view: usize,
    handler: Option<Arc<ViewEventHandler>>,
    browser: FrameNavigationState,
    browser_id: i32,
    dispatch: Arc<Dispatch>,
    cleanup_dispatch: Arc<Dispatch>,
    failure: Arc<Failure>,
  ) -> Result<Self, String> {
    let mtm = MainThreadMarker::new().ok_or_else(|| "native focus installation requires the main thread".to_string())?;
    let view = unsafe { Retained::retain(view as *mut NSView) }
      .ok_or_else(|| "native webview is unavailable".to_string())?;
    let id = NEXT_ID.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
      .map_err(|_| "native focus observer identities exhausted".to_string())?;
    let alive = Arc::new(AtomicBool::new(true));
    let callback = Arc::new(Callback { handler, browser, browser_id, dispatch: dispatch.clone(), failure });
    STATE.with(|state| {
      let mut state = state.borrow_mut();
      if state.notifications.is_none() { state.notifications = Some(notifications(mtm)); }
      state.records.push(Record {
        id, view: Weak::new(&*view), alive: alive.clone(), focused: false,
        callback: Arc::downgrade(&callback),
      });
      if let Err(error) = reconcile_windows(&mut state) {
        state.records.retain(|record| record.id != id);
        let _ = reconcile_windows(&mut state);
        if state.records.is_empty() { clear_notifications(&mut state); }
        return Err(error);
      }
      let index = state.records.len() - 1;
      state.records[index].focused = focused(&state.records[index], mtm);
      Ok(())
    })?;
    Ok(Self { id, alive, _callback: callback, cleanup_dispatch })
  }

  pub(crate) fn refresh(&self) -> Result<(), String> {
    if self.alive.load(Ordering::Acquire) { refresh_all() } else { Ok(()) }
  }
}

fn remove_observer(id: u64) {
  STATE.with(|state| {
    let mut state = state.borrow_mut();
    state.records.retain(|record| record.id != id);
    if let Err(error) = reconcile_windows(&mut state) { log::error!("native focus teardown failed: {error}"); }
    if state.records.is_empty() { clear_notifications(&mut state); }
  });
}

impl Drop for NativeFocusObserver {
  fn drop(&mut self) {
    self.alive.store(false, Ordering::Release);
    let id = self.id;
    if MainThreadMarker::new().is_some() {
      remove_observer(id);
    } else if !(self.cleanup_dispatch)(Box::new(move || remove_observer(id))) {
      log::error!("could not dispatch native focus teardown");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Mutex, atomic::AtomicUsize};

  static FORWARDED: AtomicUsize = AtomicUsize::new(0);
  static FOREIGN: AtomicUsize = AtomicUsize::new(0);

  unsafe extern "C-unwind" fn original(
    _: *mut AnyObject, _: Sel, _: *mut AnyObject,
  ) -> Bool {
    FORWARDED.fetch_add(1, Ordering::Relaxed);
    Bool::YES
  }

  unsafe extern "C-unwind" fn foreign(
    object: *mut AnyObject, selector: Sel, responder: *mut AnyObject,
  ) -> Bool {
    FOREIGN.fetch_add(1, Ordering::Relaxed);
    let class = unsafe { ffi::object_getClass(object) };
    let parent = unsafe { ffi::class_getSuperclass(class) };
    let method = unsafe { ffi::class_getInstanceMethod(parent, selector) };
    let imp = unsafe { ffi::method_getImplementation(method) }.unwrap();
    let imp: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool =
      unsafe { std::mem::transmute(imp) };
    unsafe { imp(object, selector, responder) }
  }

  #[test]
  fn later_subclass_survives_teardown_and_forwards_late_calls() {
    let base = unsafe {
      ffi::objc_allocateClassPair(NSObject::class(), c"TauriFocusTestBase".as_ptr(), 0)
    };
    assert!(!base.is_null());
    let method = unsafe { ffi::class_getInstanceMethod(NSWindow::class(), sel!(makeFirstResponder:)) };
    let implementation: Imp = unsafe {
      std::mem::transmute(original as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool)
    };
    assert!(unsafe {
      ffi::class_addMethod(base, sel!(makeFirstResponder:), implementation, ffi::method_getTypeEncoding(method))
    }.as_bool());
    unsafe { ffi::objc_registerClassPair(base); }
    let object: Retained<AnyObject> = unsafe { msg_send![&*base, new] };
    let hook = STATE.with(|state| register_class(&mut state.borrow_mut(), base).unwrap());
    let pointer = &*object as *const AnyObject as *mut AnyObject;
    unsafe { ffi::object_setClass(pointer, hook.subclass as *const AnyClass); }
    let later = unsafe {
      ffi::objc_allocateClassPair(hook.subclass as *const AnyClass, c"TauriFocusTestLater".as_ptr(), 0)
    };
    assert!(!later.is_null());
    let foreign_imp: Imp = unsafe {
      std::mem::transmute(foreign as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool)
    };
    assert!(unsafe {
      ffi::class_addMethod(later, sel!(makeFirstResponder:), foreign_imp, ffi::method_getTypeEncoding(method))
    }.as_bool());
    unsafe {
      ffi::objc_registerClassPair(later);
      ffi::object_setClass(pointer, later);
      restore_if_owned(pointer, hook.subclass, hook.original_class);
    }
    assert_eq!(unsafe { ffi::object_getClass(pointer) }, later as *const AnyClass);
    let before = FORWARDED.load(Ordering::Relaxed);
    let foreign_before = FOREIGN.load(Ordering::Relaxed);
    let result: Bool = unsafe { msg_send![&*object, makeFirstResponder: std::ptr::null_mut::<AnyObject>()] };
    assert!(result.as_bool());
    assert_eq!(FORWARDED.load(Ordering::Relaxed), before + 1);
    assert_eq!(FOREIGN.load(Ordering::Relaxed), foreign_before + 1);
  }

  #[test]
  fn revoked_capture_cannot_deliver_after_observer_is_dropped() {
    let browser = FrameNavigationState::new();
    assert!(browser.bind_browser(7));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collected = events.clone();
    let tasks: Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>> = Arc::default();
    let queued = tasks.clone();
    let callback = Arc::new(Callback {
      handler: Some(Arc::new(move |_, event| collected.lock().unwrap().push(event))),
      browser, browser_id: 7,
      dispatch: Arc::new(move |task| { queued.lock().unwrap().push(task); true }),
      failure: Arc::new(|error| panic!("unexpected focus failure: {error}")),
    });
    let alive = Arc::new(AtomicBool::new(true));
    deliver(vec![(alive.clone(), Arc::downgrade(&callback))]);
    for task in std::mem::take(&mut *tasks.lock().unwrap()) { task(); }
    assert_eq!(*events.lock().unwrap(), vec![ViewEvent::Blur]);
    deliver(vec![(alive.clone(), Arc::downgrade(&callback))]);
    alive.store(false, Ordering::Release);
    drop(callback);
    for task in std::mem::take(&mut *tasks.lock().unwrap()) { task(); }
    assert_eq!(*events.lock().unwrap(), vec![ViewEvent::Blur]);
  }
}
