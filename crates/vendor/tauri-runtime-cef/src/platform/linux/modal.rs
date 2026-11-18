// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{cell::RefCell, collections::HashMap, os::raw::c_ulong};
use gtk::prelude::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tauri::{Manager, Runtime, Window};
use x11_dl::xlib;
use super::utils::with_cef_display;

#[derive(Clone)]
struct Owner { window: gtk::ApplicationWindow, gate: crate::NativeWindowHandle, overlay: c_ulong, enabled: bool, children: usize, cleanup_claimed: bool }
#[derive(Clone)]
struct Child { owner: c_ulong, window: gtk::ApplicationWindow, gate: crate::NativeWindowHandle, previous_parent: Option<gtk::Window>, previous_modal: bool, cleanup_claimed: bool }
#[derive(Default)]
struct State { owners: HashMap<c_ulong, Owner>, children: HashMap<String, Child> }
thread_local! { static MODALS: RefCell<State> = RefCell::new(State::default()); }

struct CleanupClaim { label: String, gate: crate::NativeWindowHandle, owner: c_ulong, owner_gate: crate::NativeWindowHandle }
impl Drop for CleanupClaim {
  fn drop(&mut self) {
    MODALS.with(|state| {
      if let Some(child) = state.borrow_mut().children.get_mut(&self.label) {
        if child.gate.is_same(&self.gate) { child.cleanup_claimed = false; }
      }
      if let Some(owner) = state.borrow_mut().owners.get_mut(&self.owner) {
        if owner.gate.is_same(&self.owner_gate) { owner.cleanup_claimed = false; }
      }
    });
  }
}
fn claim_cleanup(label: &str, gate: &crate::NativeWindowHandle) -> Option<CleanupClaim> {
  MODALS.with(|state| {
    let mut state = state.borrow_mut();
    let child = state.children.get(label)?;
    if child.cleanup_claimed || !child.gate.is_same(gate) { return None; }
    let owner = child.owner;
    let owner_record = state.owners.get(&owner)?;
    if owner_record.cleanup_claimed { return None; }
    let owner_gate = owner_record.gate.clone();
    state.children.get_mut(label)?.cleanup_claimed = true;
    state.owners.get_mut(&owner)?.cleanup_claimed = true;
    Some(CleanupClaim { label: label.to_owned(), gate: gate.clone(), owner, owner_gate })
  })
}
fn failure(message: &str) -> tauri::Error {
  tauri_runtime::Error::CreateWindow(Box::new(std::io::Error::other(message.to_owned()))).into()
}
fn xid<R: Runtime>(window: &Window<R>) -> tauri::Result<c_ulong>
where R::WindowDispatcher: crate::AsCefWindowDispatcher {
  let native = crate::inactive_window::native_window_on_ui(window)?;
  let handle = native.window_handle().map_err(|_| tauri::Error::InvalidWindowHandle)?;
  match handle.as_raw() {
    RawWindowHandle::Xlib(h) => Ok(h.window as c_ulong),
    RawWindowHandle::Xcb(h) => Ok(h.window.get() as c_ulong),
    _ => Err(tauri::Error::InvalidWindowHandle),
  }
}
fn overlay(parent: c_ulong) -> tauri::Result<c_ulong> {
  with_cef_display(None, |xlib, display| unsafe {
    let mut attrs: xlib::XSetWindowAttributes = std::mem::zeroed();
    attrs.event_mask = xlib::ButtonPressMask | xlib::ButtonReleaseMask | xlib::PointerMotionMask | xlib::EnterWindowMask | xlib::LeaveWindowMask;
    let id = (xlib.XCreateWindow)(display, parent, 0, 0, 65535, 65535, 0, 0, xlib::InputOnly as u32, std::ptr::null_mut(), xlib::CWEventMask, &mut attrs);
    if id == 0 { return None; }
    (xlib.XMapRaised)(display, id);
    Some(id)
  }).ok_or_else(|| failure("could not create the X11 modal input gate"))
}
fn end_child(state: &mut State, label: &str) -> Option<(Child, Option<Owner>)> {
  let child = state.children.remove(label)?;
  let owner = state.owners.get_mut(&child.owner)?;
  owner.children -= 1;
  let removed = if owner.children == 0 { state.owners.remove(&child.owner) } else { None };
  Some((child, removed))
}
fn restore(label: &str) -> bool {
  let pair = MODALS.with(|s| {
    let state = s.borrow();
    let Some(child) = state.children.get(label) else { return Ok(None) };
    let Some(owner) = state.owners.get(&child.owner) else { return Err(()) };
    Ok(Some((child.clone(), (owner.children == 1).then(|| owner.clone()))))
  });
  let (child, owner) = match pair {
    Ok(Some(pair)) => pair,
    Ok(None) => return true,
    Err(()) => return false,
  };
  let Some(_claim) = claim_cleanup(label, &child.gate) else { return false };
  let restored = child.gate.with_gtk_window(|window| {
    window.set_modal(child.previous_modal);
    window.set_transient_for(child.previous_parent.as_ref());
    if let Some(owner) = &owner {
      return owner.gate.with_gtk_window(|window| {
        let removed = with_cef_display(false, |xlib, display| unsafe { (xlib.XDestroyWindow)(display, owner.overlay) != 0 });
        if removed { window.set_sensitive(owner.enabled); }
        removed
      }).unwrap_or(false);
    }
    true
  }).unwrap_or(false);
  if !restored { return false; }
  MODALS.with(|s| {
    let mut state = s.borrow_mut();
    if state.children.get(label).is_some_and(|current| current.gate.is_same(&child.gate)) {
      let _ = end_child(&mut state, label);
    }
  });
  true
}
fn destroyed(label: &str, expected: &crate::NativeWindowHandle) -> bool {
  let labels = MODALS.with(|s| {
    let state = s.borrow();
    let mut labels = Vec::new();
    if state.children.get(label).is_some_and(|child| child.gate.is_same(expected)) { labels.push(label.to_owned()); }
    if let Some(owner) = state.owners.iter().find_map(|(id, owner)| owner.gate.is_same(expected).then_some(*id)) {
      labels.extend(state.children.iter().filter_map(|(label, child)| (child.owner == owner).then_some(label.clone())));
    }
    labels
  });
  labels.into_iter().fold(true, |ok, label| restore(&label) & ok)
}

pub(crate) fn before_close(label: &str, window: &crate::NativeWindowHandle) -> bool {
  destroyed(label, window)
}

/// The reported window is already irreversibly closed. Only its surviving
/// counterparts may be touched, under their own original-window admission.
pub(crate) fn native_destroyed(label: &str, expected: &crate::NativeWindowHandle) -> bool {
  let actions = MODALS.with(|s| {
    let state = s.borrow();
    let owner = state.owners.iter().find_map(|(id, owner)| owner.gate.is_same(expected).then_some((*id, owner.window.as_ptr() as usize)));
    state.children.iter().filter_map(|(name, child)| {
      let child_closed = name == label && child.gate.is_same(expected);
      let owner_closed = owner.is_some_and(|(id, _)| id == child.owner);
      if !child_closed && !owner_closed { return None; }
      let owner_record = state.owners.get(&child.owner)?;
      Some((name.clone(), child.clone(), owner_record.clone(), child_closed, owner_closed, owner.map(|(_, ptr)| ptr)))
    }).collect::<Vec<_>>()
  });
  actions.into_iter().fold(true, |ok, (name, child, owner, child_closed, owner_closed, owner_pointer)| {
    let Some(_claim) = claim_cleanup(&name, &child.gate) else { return false };
    let cleaned = if child_closed {
      if owner.children != 1 || owner.gate.is_retired() { true } else {
        owner.gate.with_gtk_window(|window| {
          let removed = with_cef_display(false, |xlib, display| unsafe { (xlib.XDestroyWindow)(display, owner.overlay) != 0 });
          if removed { window.set_sensitive(owner.enabled); }
          removed
        }).unwrap_or(false)
      }
    } else if owner_closed && !child.gate.is_retired() {
      let previous = child.previous_parent.as_ref().filter(|parent| Some(parent.as_ptr() as usize) != owner_pointer);
      child.gate.with_gtk_window(|window| {
        window.set_modal(child.previous_modal);
        window.set_transient_for(previous);
      }).is_ok()
    } else { true };
    if cleaned {
      MODALS.with(|s| {
        let mut state = s.borrow_mut();
        if state.children.get(&name).is_some_and(|current| current.gate.is_same(&child.gate)) {
          let _ = end_child(&mut state, &name);
        }
      });
    }
    ok & cleaned
  })
}
pub(crate) fn begin<R: Runtime>(picker: &Window<R>, owner: &Window<R>) -> tauri::Result<()>
where R::WindowDispatcher: crate::AsCefWindowDispatcher {
  if picker.label() == owner.label() { return Err(failure("a window cannot own itself")); }
  let label = picker.label().to_owned();
  let child = picker.clone();
  let parent = owner.clone();
  picker.run_on_main_thread_blocking(move || {
    use winit::platform::gtk4::WindowExtGtk4;
    let owner_id = xid(&parent)?;
    let owner_native = crate::inactive_window::native_window_on_ui(&parent)?;
    let owner_gate = owner_native.handle();
    let owner_window = owner_native.gtk_window()
      .ok_or_else(|| failure("native GTK owner is unavailable"))?;
    let child_native = crate::inactive_window::native_window_on_ui(&child)?;
    let child_gate = child_native.handle();
    let child_window = child_native.gtk_window()
      .ok_or_else(|| failure("native GTK modal is unavailable"))?;
    let exists = MODALS.with(|s| s.borrow().owners.contains_key(&owner_id));
    if MODALS.with(|s| s.borrow().owners.get(&owner_id).is_some_and(|owner| !owner.gate.is_same(&owner_gate))) {
      return Err(failure("modal owner identity changed"));
    }
    if MODALS.with(|s| s.borrow().owners.get(&owner_id).is_some_and(|owner| owner.cleanup_claimed)) {
      return Err(failure("modal owner cleanup is in progress"));
    }
    if MODALS.with(|s| s.borrow().children.contains_key(&label)) { return Err(failure("window is already modal")); }
    let gate = if exists { None } else { Some(overlay(owner_id)?) };
    let previous_parent = child_window.transient_for();
    let previous_modal = child_window.is_modal();
    child_window.set_transient_for(Some(&owner_window));
    child_window.set_modal(true);
    let enabled = owner_window.is_sensitive();
    if !exists { owner_window.set_sensitive(false); }
    MODALS.with(|s| {
      let mut state = s.borrow_mut();
      if let Some(overlay) = gate { state.owners.insert(owner_id, Owner { window: owner_window, gate: owner_gate, overlay, enabled, children: 0, cleanup_claimed: false }); }
      state.owners.get_mut(&owner_id).expect("owner inserted").children += 1;
      state.children.insert(label, Child { owner: owner_id, window: child_window, gate: child_gate, previous_parent, previous_modal, cleanup_claimed: false });
    });
    Ok(())
  })?
}
pub(crate) fn end<R: Runtime>(picker: &Window<R>) -> tauri::Result<()>
where R::WindowDispatcher: crate::AsCefWindowDispatcher {
  let label = picker.label().to_owned();
  let original = picker.clone();
  picker.run_on_main_thread_blocking(move || {
    let native = crate::inactive_window::native_window_on_ui(&original)?;
    let expected = native.handle();
    let matches = MODALS.with(|s| s.borrow().children.get(&label)
      .is_none_or(|child| child.gate.is_same(&expected)));
    if !matches { return Err(failure("modal child identity changed")); }
    if !restore(&label) { return Err(failure("modal cleanup failed")); }
    Ok::<(), tauri::Error>(())
  })?
}
fn blocked_owner(browser_xid: c_ulong) -> Option<c_ulong> {
  let owners = MODALS.with(|s| s.borrow().owners.keys().copied().collect::<Vec<_>>());
  if owners.is_empty() || browser_xid == 0 { return None; }
  with_cef_display(None, |xlib, display| unsafe {
    let mut current = browser_xid;
    while current != 0 {
      if owners.contains(&current) { return Some(current); }
      let mut root = 0;
      let mut parent = 0;
      let mut children = std::ptr::null_mut();
      let mut count = 0;
      if (xlib.XQueryTree)(display, current, &mut root, &mut parent, &mut children, &mut count) == 0 { break; }
      if !children.is_null() { (xlib.XFree)(children.cast()); }
      if parent == current { break; }
      current = parent;
    }
    None
  })
}
pub(crate) fn blocked(browser_xid: c_ulong) -> bool {
  blocked_owner(browser_xid).is_some()
}
pub(crate) fn recover_focus(browser_xid: c_ulong) -> bool {
  let Some(owner) = blocked_owner(browser_xid) else { return false; };
  let child = MODALS.with(|s| s.borrow().children.values().find(|child| child.owner == owner).map(|child| (child.window.clone(), child.gate.clone())));
  child.is_some_and(|(child, gate)| gate.with_window(|_| child.present()).is_ok())
}
