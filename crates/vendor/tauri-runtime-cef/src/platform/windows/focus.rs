// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  collections::HashMap,
  ffi::c_void,
  sync::{Arc, LazyLock, Mutex, Weak, atomic::{AtomicBool, Ordering}, mpsc},
  thread,
};

use windows::Win32::{
  Foundation::{HWND, LPARAM, LRESULT, WPARAM},
  UI::{
    Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
    WindowsAndMessaging::{
      CWPSTRUCT, CallNextHookEx, EnumChildWindows, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_FOCUS,
      GA_ROOT, GUITHREADINFO, GetAncestor, GetForegroundWindow, GetGUIThreadInfo, GetMessageW,
      GetWindowThreadProcessId, HHOOK, IsChild, IsWindow, MSG, PM_NOREMOVE, PeekMessageW,
      PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_CALLWNDPROC,
      WINEVENT_OUTOFCONTEXT, WM_ACTIVATE, WM_ACTIVATEAPP, WM_APP, WM_CREATE, WM_DESTROY, WM_KILLFOCUS,
      WM_PARENTNOTIFY, WM_QUIT, WM_SETFOCUS,
    },
  },
};

use crate::{FrameNavigationState, ViewEvent, ViewEventHandler};

#[link(name = "kernel32")]
unsafe extern "system" {
  fn GetCurrentThreadId() -> u32;
  fn GetCurrentProcessId() -> u32;
}

pub(crate) type Dispatch = dyn Fn(Box<dyn FnOnce() + Send>) -> bool + Send + Sync + 'static;
pub(crate) type Failure = dyn Fn(String) + Send + Sync + 'static;

const PRUNE: u32 = WM_APP + 73;
#[derive(Clone, Copy, Debug)]
enum Signal {
  Gain,
  Loss,
  Unchanged,
}

#[derive(Default)]
struct FocusState {
  focused: bool,
}

impl FocusState {
  fn observe(&mut self, signal: Signal) -> bool {
    match signal {
      Signal::Gain => { self.focused = true; false }
      Signal::Loss => std::mem::replace(&mut self.focused, false),
      Signal::Unchanged => false,
    }
  }
}

fn signal(message: u32, source_inside: bool, destination_inside: bool, destination_known: bool, deactivated: bool) -> Signal {
  match message {
    WM_SETFOCUS if source_inside => Signal::Gain,
    WM_KILLFOCUS if source_inside && destination_known && !destination_inside => Signal::Loss,
    WM_ACTIVATE | WM_ACTIVATEAPP if deactivated => Signal::Loss,
    _ => Signal::Unchanged,
  }
}

struct Record {
  view: usize,
  owner_thread: u32,
  alive: Arc<AtomicBool>,
  focus: FocusState,
  handler: Option<Arc<ViewEventHandler>>,
  browser: FrameNavigationState,
  browser_id: i32,
  dispatch: Arc<Dispatch>,
  failure: Arc<Failure>,
  threads: HashMap<u32, Arc<HookWorker>>,
  windows: Vec<usize>,
}

impl Record {
  fn contains(&self, hwnd: HWND) -> bool {
    let view = HWND(self.view as *mut c_void);
    !hwnd.0.is_null() && (hwnd == view || unsafe { IsChild(view, hwnd).as_bool() })
  }

  fn native_focus(&self) -> Result<bool, String> {
    let view = HWND(self.view as *mut c_void);
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() || unsafe { GetAncestor(view, GA_ROOT) } != foreground {
      return Ok(false);
    }
    let mut info = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    unsafe { GetGUIThreadInfo(0, &mut info) }
      .map_err(|error| format!("failed to observe foreground focus: {error}"))?;
    Ok(self.contains(info.hwndFocus))
  }

  fn synchronize_threads(&mut self) -> Result<(), String> {
    let view = HWND(self.view as *mut c_void);
    if !unsafe { IsWindow(Some(view)).as_bool() } {
      return Err("native webview window is unavailable".into());
    }
    let mut descendants: Vec<(usize, u32, u32)> = Vec::new();
    unsafe {
      let _ = EnumChildWindows(Some(view), Some(collect_child), LPARAM(&mut descendants as *mut _ as isize));
    }
    let process = unsafe { GetCurrentProcessId() };
    let mut threads = HashMap::new();
    threads.insert(self.owner_thread, worker(self.owner_thread)?);
    let root = unsafe { GetAncestor(view, GA_ROOT) };
    let mut root_process = 0;
    let root_thread = unsafe { GetWindowThreadProcessId(root, Some(&mut root_process)) };
    if root_thread == 0 || root_process != process {
      return Err("native webview root window is unavailable or owned by another process".into());
    }
    if !threads.contains_key(&root_thread) { threads.insert(root_thread, worker(root_thread)?); }
    let mut windows = Vec::with_capacity(descendants.len() + 1);
    windows.push(self.view);
    for (window, thread, owner) in descendants {
      if owner != process {
        return Err("native webview has a descendant HWND owned by another process".into());
      }
      windows.push(window);
      if !threads.contains_key(&thread) { threads.insert(thread, worker(thread)?); }
    }
    self.threads = threads;
    self.windows = windows;
    Ok(())
  }
}

unsafe extern "system" fn collect_child(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
  let descendants = unsafe { &mut *(data.0 as *mut Vec<(usize, u32, u32)>) };
  let mut process = 0;
  let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
  if thread != 0 { descendants.push((hwnd.0 as usize, thread, process)); }
  windows::core::BOOL(1)
}

type Delivery = (Arc<AtomicBool>, Option<Arc<ViewEventHandler>>, FrameNavigationState, i32, Arc<Dispatch>);
static RECORDS: LazyLock<Mutex<Vec<Record>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static WORKERS: LazyLock<Mutex<HashMap<u32, Weak<HookWorker>>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

fn records() -> &'static Mutex<Vec<Record>> {
  &RECORDS
}

fn deliver(events: Vec<Delivery>) {
  for (alive, handler, browser, browser_id, dispatch) in events {
    if !(dispatch)(Box::new(move || {
      if alive.load(Ordering::Acquire) {
        crate::view_event::emit_view_event(&handler, &browser, browser_id, false, ViewEvent::Blur);
      }
    })) {
      log::error!("failed to dispatch native webview blur");
    }
  }
}

fn fail(dispatch: Arc<Dispatch>, failure: Arc<Failure>, error: String) {
  if !(dispatch)(Box::new(move || failure(error))) {
    log::error!("failed to dispatch native focus observer failure");
  }
}

// A thread-scoped WH_CALLWNDPROC callback runs on the native window's UI thread
// before the window procedure. It carries the actual focus destination, rather
// than a snapshot taken after an asynchronous WinEvent has been queued.
unsafe extern "system" fn window_message(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
  if code >= 0 && lparam.0 != 0 {
    let message = unsafe { &*(lparam.0 as *const CWPSTRUCT) };
    if matches!(message.message, WM_SETFOCUS | WM_KILLFOCUS | WM_ACTIVATE | WM_ACTIVATEAPP | WM_PARENTNOTIFY) {
      let owner_thread = unsafe { GetCurrentThreadId() };
      let mut deliveries = Vec::new();
      let mut failures = Vec::new();
      {
        let mut records = records().lock().unwrap_or_else(|error| error.into_inner());
        records.retain(|record| record.alive.load(Ordering::Acquire));
        for record in records.iter_mut().filter(|record| record.threads.contains_key(&owner_thread)) {
          if !record.browser.has_browser_id(record.browser_id) { continue; }
          let source_inside = record.contains(message.hwnd);
          if message.message == WM_PARENTNOTIFY && source_inside
            && matches!((message.wParam.0 & 0xffff) as u32, WM_CREATE | WM_DESTROY)
          {
            if let Err(error) = record.synchronize_threads() {
              failures.push((record.dispatch.clone(), record.failure.clone(), error));
            }
            continue;
          }
          let destination = HWND(message.wParam.0 as *mut c_void);
          let destination_inside = message.message == WM_KILLFOCUS && record.contains(destination);
          let root = unsafe { GetAncestor(HWND(record.view as *mut c_void), GA_ROOT) };
          let deactivated = (message.message == WM_ACTIVATEAPP && message.wParam.0 == 0
            || message.message == WM_ACTIVATE && (message.wParam.0 & 0xffff) == 0)
            && !root.0.is_null() && root == message.hwnd;
          let active_source = source_inside && (message.message != WM_SETFOCUS
            || !root.0.is_null() && unsafe { GetForegroundWindow() } == root);
          if record.focus.observe(signal(message.message, active_source, destination_inside, !destination.0.is_null(), deactivated)) {
            deliveries.push((record.alive.clone(), record.handler.clone(), record.browser.clone(), record.browser_id, record.dispatch.clone()));
          }
        }
      }
      deliver(deliveries);
      for (dispatch, failure, error) in failures { fail(dispatch, failure, error); }
    }
  }
  unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

struct HookWorker {
  thread: u32,
  stopping: Arc<AtomicBool>,
}

impl Drop for HookWorker {
  fn drop(&mut self) {
    self.stopping.store(true, Ordering::Release);
    let _ = unsafe { PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
  }
}

fn worker(owner_thread: u32) -> Result<Arc<HookWorker>, String> {
  let mut workers = WORKERS
    .lock().unwrap_or_else(|error| error.into_inner());
  if let Some(worker) = workers.get(&owner_thread).and_then(Weak::upgrade) { return Ok(worker); }
  let (ready_tx, ready_rx) = mpsc::sync_channel(1);
  let stopping = Arc::new(AtomicBool::new(false));
  let worker_stopping = stopping.clone();
  thread::Builder::new().name("cef-native-focus".into()).spawn(move || {
    let mut message = MSG::default();
    unsafe { let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE); }
    let hook: HHOOK = match unsafe { SetWindowsHookExW(WH_CALLWNDPROC, Some(window_message), None, owner_thread) } {
      Ok(hook) => hook,
      Err(error) => {
        let _ = ready_tx.send(Err(format!("failed to install native focus hook: {error}")));
        return;
      }
    };
    if ready_tx.send(Ok(unsafe { GetCurrentThreadId() })).is_ok() {
      while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        records().lock().unwrap_or_else(|error| error.into_inner())
          .retain(|record| record.alive.load(Ordering::Acquire));
        if worker_stopping.load(Ordering::Acquire) { break; }
      }
    }
    if let Err(error) = unsafe { UnhookWindowsHookEx(hook) } {
      log::error!("failed to remove native focus hook: {error}");
    }
    records().lock().unwrap_or_else(|error| error.into_inner())
      .retain(|record| record.alive.load(Ordering::Acquire));
  }).map_err(|error| format!("failed to start native focus observer: {error}"))?;
  let thread = ready_rx.recv().map_err(|_| "native focus observer exited during startup".to_string())??;
  let worker = Arc::new(HookWorker { thread, stopping });
  workers.insert(owner_thread, Arc::downgrade(&worker));
  Ok(worker)
}

struct MonitorRegistration {
  alive: Weak<AtomicBool>,
  dispatch: Arc<Dispatch>,
  failure: Arc<Failure>,
}

static MONITORS: LazyLock<Mutex<HashMap<usize, MonitorRegistration>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
  static MONITOR_ALIVE: std::cell::RefCell<Option<Weak<AtomicBool>>> = const { std::cell::RefCell::new(None) };
}

unsafe extern "system" fn child_event(
  hook: HWINEVENTHOOK, event: u32, hwnd: HWND, object: i32, child: i32, _: u32, _: u32,
) {
  if hwnd.0.is_null() || (event != EVENT_OBJECT_FOCUS && (object != 0 || child != 0)) { return; }
  let thread_alive = MONITOR_ALIVE.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade));
  let registration = {
    let monitors = MONITORS.lock().unwrap_or_else(|error| error.into_inner());
    monitors.get(&(hook.0 as usize)).and_then(|entry| {
      entry.alive.upgrade().and_then(|alive| {
        thread_alive.as_ref().filter(|expected| Arc::ptr_eq(expected, &alive))
          .map(|_| (alive, entry.dispatch.clone(), entry.failure.clone()))
      })
    })
  };
  let Some((alive, dispatch, failure)) = registration else { return };
  if !alive.load(Ordering::Acquire) { return; }
  let observation = {
    let records = records().lock().unwrap_or_else(|error| error.into_inner());
    records.iter().find(|record| Arc::ptr_eq(&record.alive, &alive)).and_then(|record| {
      match event {
        EVENT_OBJECT_CREATE if record.contains(hwnd) => Some(true),
        EVENT_OBJECT_DESTROY if record.windows.contains(&(hwnd.0 as usize)) => Some(true),
        EVENT_OBJECT_FOCUS => {
          let view_root = unsafe { GetAncestor(HWND(record.view as *mut c_void), GA_ROOT) };
          if view_root.0.is_null() || unsafe { GetAncestor(hwnd, GA_ROOT) } != view_root {
            None
          } else {
            Some(record.contains(hwnd) && !record.windows.contains(&(hwnd.0 as usize)))
          }
        }
        _ => None,
      }
    })
  };
  if let Some(rescan) = observation {
    let weak = Arc::downgrade(&alive);
    if !(dispatch)(Box::new(move || {
      if let Some(alive) = weak.upgrade() && alive.load(Ordering::Acquire) {
        if let Err(error) = refresh_token(&alive, rescan) { failure(error); }
      }
    })) {
      log::error!("failed to dispatch native descendant refresh");
    }
  }
}

struct MonitorWorker {
  thread: u32,
  stopping: Arc<AtomicBool>,
}

impl Drop for MonitorWorker {
  fn drop(&mut self) {
    self.stopping.store(true, Ordering::Release);
    let _ = unsafe { PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
  }
}

fn monitor(alive: &Arc<AtomicBool>, dispatch: Arc<Dispatch>, failure: Arc<Failure>) -> Result<MonitorWorker, String> {
  let alive = Arc::downgrade(alive);
  let (ready_tx, ready_rx) = mpsc::sync_channel(1);
  let stopping = Arc::new(AtomicBool::new(false));
  let worker_stopping = stopping.clone();
  thread::Builder::new().name("cef-focus-children".into()).spawn(move || {
    let mut message = MSG::default();
    unsafe { let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE); }
    let hook = unsafe {
      SetWinEventHook(EVENT_OBJECT_CREATE, EVENT_OBJECT_FOCUS, None, Some(child_event), 0, 0, WINEVENT_OUTOFCONTEXT)
    };
    if hook.is_invalid() {
      let _ = ready_tx.send(Err("failed to observe native child window lifetime".to_string()));
      return;
    }
    MONITOR_ALIVE.with(|slot| *slot.borrow_mut() = Some(alive.clone()));
    MONITORS.lock().unwrap_or_else(|error| error.into_inner()).insert(
      hook.0 as usize, MonitorRegistration { alive, dispatch, failure },
    );
    if ready_tx.send(Ok(unsafe { GetCurrentThreadId() })).is_ok() {
      while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if worker_stopping.load(Ordering::Acquire) { break; }
      }
    }
    MONITORS.lock().unwrap_or_else(|error| error.into_inner()).remove(&(hook.0 as usize));
    MONITOR_ALIVE.with(|slot| { slot.borrow_mut().take(); });
    if !unsafe { UnhookWinEvent(hook).as_bool() } {
      log::error!("failed to remove native child window lifetime hook");
    }
  }).map_err(|error| format!("failed to start native child observer: {error}"))?;
  let thread = ready_rx.recv().map_err(|_| "native child observer exited during startup".to_string())??;
  Ok(MonitorWorker { thread, stopping })
}

/// Registration and callbacks belong to one native browser lifetime.
pub(crate) struct NativeFocusObserver {
  alive: Arc<AtomicBool>,
  worker: Arc<HookWorker>,
  _monitor: MonitorWorker,
}

impl NativeFocusObserver {
  pub(crate) fn install(
    hwnd: usize,
    handler: Option<Arc<ViewEventHandler>>,
    browser: FrameNavigationState,
    browser_id: i32,
    dispatch: Arc<Dispatch>,
    failure: Arc<Failure>,
  ) -> Result<Self, String> {
    let owner_thread = unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut c_void), None) };
    if owner_thread == 0 || owner_thread != unsafe { GetCurrentThreadId() } {
      return Err("native focus observer must be installed on its window's UI thread".into());
    }
    let alive = Arc::new(AtomicBool::new(true));
    let monitor = monitor(&alive, dispatch.clone(), failure.clone())?;
    let mut record = Record {
      view: hwnd, owner_thread, alive: alive.clone(), focus: FocusState::default(),
      handler, browser, browser_id, dispatch, failure, threads: HashMap::new(), windows: Vec::new(),
    };
    record.synchronize_threads()?;
    record.focus.focused = record.native_focus()?;
    let worker = record.threads.get(&owner_thread).cloned()
      .ok_or_else(|| "native focus owner thread was not registered".to_string())?;
    records().lock().unwrap_or_else(|error| error.into_inner()).push(record);
    Ok(Self { alive, worker, _monitor: monitor })
  }

  /// Reparenting is performed on the owning UI thread. Observe the native
  /// focus there, in the same ordering domain as the window-message hook.
  pub(crate) fn refresh(&self) -> Result<(), String> {
    refresh_token(&self.alive, true)
  }
}

fn refresh_token(alive: &Arc<AtomicBool>, rescan: bool) -> Result<(), String> {
  let mut deliveries = Vec::new();
  {
    let mut records = records().lock().unwrap_or_else(|error| error.into_inner());
    for record in records.iter_mut().filter(|record| Arc::ptr_eq(&record.alive, alive)) {
      if !record.alive.load(Ordering::Acquire) || !record.browser.has_browser_id(record.browser_id) { continue; }
      if record.owner_thread != unsafe { GetCurrentThreadId() } {
        return Err("native focus refresh was not called on the window UI thread".into());
      }
      if rescan { record.synchronize_threads()?; }
      let now = record.native_focus()?;
      if record.focus.observe(if now { Signal::Gain } else { Signal::Loss }) {
        deliveries.push((record.alive.clone(), record.handler.clone(), record.browser.clone(), record.browser_id, record.dispatch.clone()));
      }
    }
  }
  deliver(deliveries);
  Ok(())
}

impl Drop for NativeFocusObserver {
  fn drop(&mut self) {
    self.alive.store(false, Ordering::Release);
    let _ = unsafe { PostThreadMessageW(self.worker.thread, PRUNE, WPARAM(0), LPARAM(0)) };
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn revoked_capture_does_not_reach_replacement_with_reused_ids() {
    let old = FrameNavigationState::new();
    let replacement = FrameNavigationState::new();
    assert!(old.bind_browser(7));
    assert!(replacement.bind_browser(7));
    let current = Arc::new(Mutex::new(Some(old.clone())));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let seen = captured.clone();
    let handler: Option<Arc<ViewEventHandler>> =
      Some(Arc::new(move |_, event| seen.lock().unwrap().push(event)));
    let queued: Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>> = Arc::default();
    let jobs = queued.clone();
    let generation = old.clone();
    let owner = current.clone();
    let dispatch: Arc<Dispatch> = Arc::new(move |task| {
      let owner = owner.clone();
      let generation = generation.clone();
      jobs.lock().unwrap().push(Box::new(move || {
        let current = owner.lock().unwrap().clone();
        if current.as_ref().is_some_and(|state| state.is_same_browser(&generation)) {
          task();
        }
      }));
      true
    });
    deliver(vec![(Arc::new(AtomicBool::new(true)), handler.clone(), old.clone(), 7, dispatch.clone())]);
    for task in std::mem::take(&mut *queued.lock().unwrap()) { task(); }
    assert_eq!(*captured.lock().unwrap(), vec![ViewEvent::Blur]);
    deliver(vec![(Arc::new(AtomicBool::new(true)), handler, old, 7, dispatch)]);
    *current.lock().unwrap() = Some(replacement);
    for task in std::mem::take(&mut *queued.lock().unwrap()) { task(); }
    assert_eq!(*captured.lock().unwrap(), vec![ViewEvent::Blur]);
  }

  #[test]
  fn activation_focus_transfer_and_duplicate_loss() {
    let mut state = FocusState::default();
    assert!(!state.observe(signal(WM_SETFOCUS, true, false, true, false)));
    assert!(!state.observe(signal(WM_KILLFOCUS, true, true, true, false)));
    assert!(state.observe(signal(WM_ACTIVATEAPP, false, false, false, true)));
    assert!(!state.observe(signal(WM_KILLFOCUS, true, false, true, false)));
    assert!(!state.observe(signal(WM_SETFOCUS, true, false, true, false)));
    assert!(!state.observe(signal(WM_KILLFOCUS, true, false, false, false)));
    assert!(state.observe(signal(WM_KILLFOCUS, true, false, true, false)));
    assert!(!state.observe(signal(WM_KILLFOCUS, true, false, true, false)));
  }

  #[test]
  fn bounded_signal_sequences_match_the_focus_transition_model() {
    let signals = [Signal::Gain, Signal::Loss, Signal::Unchanged];
    for mut sequence in 0..3_usize.pow(7) {
      let mut implementation = FocusState::default();
      let mut expected_focused = false;
      for _ in 0..7 {
        let input = signals[sequence % 3];
        sequence /= 3;
        let expected_loss = matches!(input, Signal::Loss) && expected_focused;
        match input {
          Signal::Gain => expected_focused = true,
          Signal::Loss => expected_focused = false,
          Signal::Unchanged => {}
        }
        assert_eq!(implementation.observe(input), expected_loss);
      }
    }
  }
}

#[cfg(test)]
mod native_tests {
  use super::*;
  use windows::{
    core::w,
    Win32::UI::{
      Input::KeyboardAndMouse::{GetFocus, SetFocus},
      WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SetForegroundWindow, SetParent, WINDOW_EX_STYLE,
        WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
      },
    },
  };

  struct Windows(Vec<HWND>);
  impl Drop for Windows {
    fn drop(&mut self) {
      for window in self.0.drain(..) { let _ = unsafe { DestroyWindow(window) }; }
    }
  }

  unsafe fn window(parent: Option<HWND>) -> HWND {
    unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE(0), w!("STATIC"), w!("focus fixture"),
        if parent.is_some() { WS_CHILD | WS_VISIBLE } else { WS_OVERLAPPEDWINDOW | WS_VISIBLE },
        0, 0, 120, 80, parent, None, None, None,
      )
    }.expect("native test window")
  }

  #[test]
  fn native_focus_transfer_activation_reparent_and_teardown() {
    let first = unsafe { window(None) };
    let second = unsafe { window(None) };
    let _windows = Windows(vec![first, second]);
    let view = unsafe { window(Some(first)) };
    let descendant = unsafe { window(Some(view)) };
    let other = unsafe { window(Some(first)) };
    assert!(unsafe { SetForegroundWindow(first).as_bool() });
    let _ = unsafe { SetFocus(Some(descendant)) };
    assert_eq!(unsafe { GetFocus() }, descendant);

    let browser = FrameNavigationState::new();
    assert!(browser.bind_browser(7));
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let failures = errors.clone();
    let queued: Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>> = Arc::default();
    let pending = queued.clone();
    let owner = unsafe { GetCurrentThreadId() };
    let dispatch: Arc<Dispatch> = Arc::new(move |task| {
      if unsafe { GetCurrentThreadId() } == owner { task(); }
      else { pending.lock().unwrap().push(task); }
      true
    });
    let observer = NativeFocusObserver::install(
      view.0 as usize,
      Some(Arc::new(move |_, event| seen.lock().unwrap().push(event))),
      browser, 7, dispatch, Arc::new(move |error| failures.lock().unwrap().push(error)),
    ).expect("native focus observer");

    let _ = unsafe { SetFocus(Some(view)) };
    assert_eq!(unsafe { GetFocus() }, view);
    assert!(events.lock().unwrap().is_empty());
    let _ = unsafe { SetFocus(Some(other)) };
    assert_eq!(unsafe { GetFocus() }, other);
    assert_eq!(*events.lock().unwrap(), vec![ViewEvent::Blur]);
    let _ = unsafe { SetFocus(Some(descendant)) };
    assert_eq!(unsafe { GetFocus() }, descendant);
    assert!(unsafe { SetForegroundWindow(second).as_bool() });
    assert_eq!(*events.lock().unwrap(), vec![ViewEvent::Blur, ViewEvent::Blur]);

    assert!(unsafe { SetForegroundWindow(first).as_bool() });
    let _ = unsafe { SetFocus(Some(descendant)) };
    assert_eq!(unsafe { GetFocus() }, descendant);
    unsafe { SetParent(view, Some(second)) }.expect("reparent native view");
    observer.refresh().expect("refresh reparented focus");
    for task in std::mem::take(&mut *queued.lock().unwrap()) { task(); }
    assert_eq!(*events.lock().unwrap(), vec![ViewEvent::Blur, ViewEvent::Blur, ViewEvent::Blur]);
    drop(observer);
    for task in std::mem::take(&mut *queued.lock().unwrap()) { task(); }
    assert_eq!(*events.lock().unwrap(), vec![ViewEvent::Blur, ViewEvent::Blur, ViewEvent::Blur]);
    assert!(errors.lock().unwrap().is_empty());
  }
}
