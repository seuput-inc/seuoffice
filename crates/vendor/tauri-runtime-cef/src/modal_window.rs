// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#[cfg(any(target_os = "macos", windows))]
mod native {
  use std::{cell::RefCell, collections::HashMap};
  use tauri::{Manager, Runtime, Window};

  #[cfg(target_os = "macos")]
  type Handle = objc2::rc::Retained<objc2_app_kit::NSWindow>;
  #[cfg(windows)]
  type Handle = windows::Win32::Foundation::HWND;

  struct Owner { handle: Handle, gate: Option<crate::NativeWindowHandle>, was_enabled: bool, children: usize, cleanup_claimed: bool }
  struct Child { owner: String, handle: Handle, gate: Option<crate::NativeWindowHandle>, cleanup_claimed: bool }
  #[derive(Default)]
  struct Modals { owners: HashMap<String, Owner>, children: HashMap<String, Child> }
  thread_local! { static MODALS: RefCell<Modals> = RefCell::new(Modals::default()); }

  struct CleanupClaim { label: String, gate: crate::NativeWindowHandle, owner: String, owner_gate: crate::NativeWindowHandle }
  impl Drop for CleanupClaim {
    fn drop(&mut self) {
      MODALS.with(|state| {
        if let Some(child) = state.borrow_mut().children.get_mut(&self.label) {
          if child.gate.as_ref().is_some_and(|gate| gate.is_same(&self.gate)) { child.cleanup_claimed = false; }
        }
        if let Some(owner) = state.borrow_mut().owners.get_mut(&self.owner) {
          if owner.gate.as_ref().is_some_and(|gate| gate.is_same(&self.owner_gate)) { owner.cleanup_claimed = false; }
        }
      });
    }
  }
  fn claim_cleanup(label: &str, gate: &crate::NativeWindowHandle) -> Option<CleanupClaim> {
    MODALS.with(|state| {
      let mut state = state.borrow_mut();
      let child = state.children.get(label)?;
      if child.cleanup_claimed || child.gate.as_ref().is_none_or(|actual| !actual.is_same(gate)) { return None; }
      let owner = child.owner.clone();
      let owner_record = state.owners.get(&owner)?;
      if owner_record.cleanup_claimed { return None; }
      let owner_gate = owner_record.gate.clone()?;
      state.children.get_mut(label)?.cleanup_claimed = true;
      state.owners.get_mut(&owner)?.cleanup_claimed = true;
      Some(CleanupClaim { label: label.to_owned(), gate: gate.clone(), owner, owner_gate })
    })
  }

  fn failure(message: &str) -> tauri::Error {
    tauri_runtime::Error::CreateWindow(Box::new(std::io::Error::other(message.to_owned()))).into()
  }

  #[cfg(target_os = "macos")]
  fn handle(native: &dyn winit::window::Window) -> tauri::Result<Handle> {
    Ok(crate::platform::macos::nswindow(native))
  }
  #[cfg(windows)]
  fn handle(native: &dyn winit::window::Window) -> tauri::Result<Handle> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = native.window_handle().map_err(|_| tauri::Error::InvalidWindowHandle)?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return Err(tauri::Error::InvalidWindowHandle) };
    Ok(windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _))
  }

  #[cfg(target_os = "macos")]
  fn attach(owner: &Handle, child: &Handle, already_owned: bool) -> tauri::Result<bool> {
    if !already_owned && owner.attachedSheet().is_some() { return Err(failure("owner already has a sheet")); }
    owner.beginSheet_completionHandler(child, None);
    Ok(true)
  }
  #[cfg(windows)]
  fn attach(owner: &Handle, _child: &Handle, already_owned: bool) -> tauri::Result<bool> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
    let enabled = unsafe { IsWindowEnabled(*owner) }.as_bool();
    if !already_owned && enabled { unsafe { let _ = EnableWindow(*owner, false); } }
    Ok(enabled)
  }
  #[cfg(target_os = "macos")]
  fn detach(owner: &Handle, child: &Handle, _restore: bool) { owner.endSheet(child); }
  #[cfg(windows)]
  fn detach(owner: &Handle, _child: &Handle, restore: bool) {
    if restore { unsafe { let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(*owner, true); } }
  }

  fn end_child(state: &mut Modals, label: &str) -> Option<(Handle, Handle, bool, Option<crate::NativeWindowHandle>, Option<crate::NativeWindowHandle>)> {
    let child = state.children.remove(label)?;
    let owner = state.owners.get_mut(&child.owner)?;
    owner.children -= 1;
    let last = owner.children == 0;
    let result = (owner.handle.clone(), child.handle, last && owner.was_enabled, owner.gate.clone(), child.gate);
    if last { state.owners.remove(&child.owner); }
    Some(result)
  }
  fn try_end_child(label: &str) -> bool {
    let action = MODALS.with(|state| {
      let state = state.borrow();
      let Some(child) = state.children.get(label) else { return Ok(None) };
      let Some(owner) = state.owners.get(&child.owner) else { return Err(()) };
      let Some(owner_gate) = owner.gate.clone() else { return Err(()) };
      let Some(child_gate) = child.gate.clone() else { return Err(()) };
      Ok(Some((owner.handle.clone(), child.handle.clone(), owner.children == 1 && owner.was_enabled, owner_gate, child_gate)))
    });
    let (owner, child, restore, owner_gate, child_gate) = match action {
      Ok(Some(action)) => action,
      Ok(None) => return true,
      Err(()) => return false,
    };
    let Some(_claim) = claim_cleanup(label, &child_gate) else { return false };
    if !owner_gate.with_window(|_| child_gate.with_window(|_| detach(&owner, &child, restore)))
      .is_ok_and(|result| result.is_ok()) {
      return false;
    }
    MODALS.with(|state| {
      let mut state = state.borrow_mut();
      if state.children.get(label).and_then(|child| child.gate.as_ref()).is_some_and(|gate| gate.is_same(&child_gate)) {
        let _ = end_child(&mut state, label);
      }
    });
    true
  }

  pub(super) fn destroyed(label: &str, expected: &crate::NativeWindowHandle) -> bool {
    let labels = MODALS.with(|state| {
      let state = state.borrow();
      let mut labels = Vec::new();
      if state.children.get(label).and_then(|child| child.gate.as_ref()).is_some_and(|gate| gate.is_same(expected)) {
        labels.push(label.to_owned());
      }
      let owner_matches = state.owners.get(label).and_then(|owner| owner.gate.as_ref()).is_some_and(|gate| gate.is_same(expected));
      labels.extend(state.children.iter().filter_map(|(key, child)| (owner_matches && child.owner == label).then_some(key.clone())));
      labels
    });
    labels.into_iter().fold(true, |ok, label| try_end_child(&label) & ok)
  }

  #[cfg(windows)]
  pub(super) fn native_destroyed(label: &str, expected: &crate::NativeWindowHandle) -> bool {
    let actions = MODALS.with(|state| {
      let state = state.borrow();
      let owner_closed = state.owners.get(label).and_then(|owner| owner.gate.as_ref()).is_some_and(|gate| gate.is_same(expected));
      state.children.iter().filter_map(|(name, child)| {
        let child_closed = name == label && child.gate.as_ref().is_some_and(|gate| gate.is_same(expected));
        if !child_closed && !(owner_closed && child.owner == label) { return None; }
        let owner = state.owners.get(&child.owner)?;
        Some((name.clone(), child.gate.clone(), owner.gate.clone(), child_closed && owner.children == 1 && owner.was_enabled))
      }).collect::<Vec<_>>()
    });
    actions.into_iter().fold(true, |ok, (name, child, owner, restore)| {
      let Some(_claim) = child.as_ref().and_then(|gate| claim_cleanup(&name, gate)) else { return false };
      let cleaned = if !restore { true } else if let Some(owner) = owner {
        owner.is_retired() || owner.with_hwnd(|hwnd| unsafe {
          let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(hwnd, true);
        }).is_ok()
      } else { false };
      if cleaned {
        MODALS.with(|state| {
          let mut state = state.borrow_mut();
          if state.children.get(&name).and_then(|entry| entry.gate.as_ref())
            .zip(child.as_ref()).is_some_and(|(actual, expected)| actual.is_same(expected)) {
            let _ = end_child(&mut state, &name);
          }
        });
      }
      ok & cleaned
    })
  }


  #[cfg(target_os = "macos")]
  pub(super) fn will_close(label: &str, expected: &crate::NativeWindowHandle, closing: &objc2_app_kit::NSWindow) -> bool {
    let actions = MODALS.with(|state| {
      let state = state.borrow();
      let owner_closed = state.owners.get(label).and_then(|owner| owner.gate.as_ref()).is_some_and(|gate| gate.is_same(expected));
      state.children.iter().filter_map(|(name, child)| {
        let child_closed = name == label && child.gate.as_ref().is_some_and(|gate| gate.is_same(expected));
        if !child_closed && !(owner_closed && child.owner == label) { return None; }
        let counterpart = if child_closed { state.owners.get(&child.owner).and_then(|owner| owner.gate.clone()) } else { child.gate.clone() };
        Some((name.clone(), child.gate.clone(), counterpart, child_closed))
      }).collect::<Vec<_>>()
    });
    actions.into_iter().fold(true, |ok, (name, child, counterpart, child_closed)| {
      let Some(_claim) = child.as_ref().and_then(|gate| claim_cleanup(&name, gate)) else { return false };
      let cleaned = if let Some(counterpart) = counterpart {
        if counterpart.is_retired() { true } else {
          counterpart.with_ns_window(|window| {
            if child_closed { window.endSheet(closing); } else { closing.endSheet(window); }
          }).is_ok()
        }
      } else { false };
      if cleaned {
        MODALS.with(|state| {
          let mut state = state.borrow_mut();
          if state.children.get(&name).and_then(|entry| entry.gate.as_ref())
            .zip(child.as_ref()).is_some_and(|(actual, expected)| actual.is_same(expected)) {
            let _ = end_child(&mut state, &name);
          }
        });
      }
      ok & cleaned
    })
  }
  pub(super) fn begin<R: Runtime>(picker: &Window<R>, owner: &Window<R>) -> tauri::Result<()>
  where R::WindowDispatcher: crate::AsCefWindowDispatcher {
    if picker.label() == owner.label() { return Err(failure("a window cannot be its own modal owner")); }
    let child_label = picker.label().to_owned();
    let owner_label = owner.label().to_owned();
    let child = picker.clone();
    let parent = owner.clone();
    picker.run_on_main_thread_blocking(move || {
      let child_native = crate::inactive_window::native_window_on_ui(&child)?;
      let parent_native = crate::inactive_window::native_window_on_ui(&parent)?;
      let child_handle = handle(child_native.as_ref())?;
      let parent_handle = handle(parent_native.as_ref())?;
      let child_gate = child_native.handle();
      let parent_gate = parent_native.handle();
      MODALS.with(|state| {
        let mut state = state.borrow_mut();
        if state.children.contains_key(&child_label) { return Err(failure("window is already modal")); }
        if state.owners.get(&owner_label).is_some_and(|owner| owner.gate.as_ref().is_none_or(|gate| !gate.is_same(&parent_gate))) {
          return Err(failure("modal owner identity changed"));
        }
        if state.owners.get(&owner_label).is_some_and(|owner| owner.cleanup_claimed) { return Err(failure("modal owner cleanup is in progress")); }
        let exists = state.owners.contains_key(&owner_label);
        let enabled = attach(&parent_handle, &child_handle, exists)?;
        state.owners.entry(owner_label.clone()).or_insert(Owner { handle: parent_handle, gate: Some(parent_gate), was_enabled: enabled, children: 0, cleanup_claimed: false }).children += 1;
        state.children.insert(child_label, Child { owner: owner_label, handle: child_handle, gate: Some(child_gate), cleanup_claimed: false });
        Ok(())
      })
    })?
  }
  pub(super) fn end<R: Runtime>(picker: &Window<R>) -> tauri::Result<()>
  where R::WindowDispatcher: crate::AsCefWindowDispatcher {
    let label = picker.label().to_owned();
    let original = picker.clone();
    picker.run_on_main_thread_blocking(move || {
      let native = crate::inactive_window::native_window_on_ui(&original)?;
      let expected = native.handle();
      let matches = MODALS.with(|state| state.borrow().children.get(&label)
        .and_then(|child| child.gate.as_ref()).is_none_or(|gate| gate.is_same(&expected)));
      if !matches { return Err(failure("modal child identity changed")); }
      if !try_end_child(&label) { return Err(failure("modal cleanup failed")); }
      Ok::<(), tauri::Error>(())
    })?
  }

  #[cfg(all(test, windows))]
  mod tests {
    use super::*;
    #[test]
    fn last_child_restores_only_a_previously_enabled_owner() {
      for initially_enabled in [false, true] {
        let mut state = Modals::default();
        let handle = Handle::default();
        state.owners.insert("owner".into(), Owner { handle, gate: None, was_enabled: initially_enabled, children: 2, cleanup_claimed: false });
        for label in ["first", "second"] {
          state.children.insert(label.into(), Child { owner: "owner".into(), handle, gate: None, cleanup_claimed: false });
        }
        assert!(!end_child(&mut state, "first").unwrap().2);
        assert!(end_child(&mut state, "first").is_none());
        assert_eq!(end_child(&mut state, "second").unwrap().2, initially_enabled);
        assert!(state.owners.is_empty());
      }
    }
  }
}

#[cfg(target_os = "macos")]
pub(crate) fn will_close_macos(label: &str, window: &crate::NativeWindowHandle, native: &objc2_app_kit::NSWindow) -> bool {
  native::will_close(label, window, native)
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn before_close(label: &str, window: &crate::NativeWindowHandle) -> bool {
  native::destroyed(label, window)
}

#[cfg(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
pub(crate) fn before_close(label: &str, window: &crate::NativeWindowHandle) -> bool {
  crate::platform::linux::before_close_modal(label, window)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn native_destroyed(label: &str, window: &crate::NativeWindowHandle) -> bool {
  #[cfg(windows)]
  { native::native_destroyed(label, window) }
  #[cfg(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
  { crate::platform::linux::native_destroyed_modal(label, window) }
}

pub(crate) fn begin<R: tauri::Runtime>(picker: &tauri::Window<R>, owner: &tauri::Window<R>) -> tauri::Result<()>
where R::WindowDispatcher: crate::AsCefWindowDispatcher {
  #[cfg(any(target_os = "macos", windows))]
  { native::begin(picker, owner) }
  #[cfg(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
  { crate::platform::linux::begin_modal(picker, owner) }
}
pub(crate) fn end<R: tauri::Runtime>(picker: &tauri::Window<R>) -> tauri::Result<()>
where R::WindowDispatcher: crate::AsCefWindowDispatcher {
  #[cfg(any(target_os = "macos", windows))]
  { native::end(picker) }
  #[cfg(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
  { crate::platform::linux::end_modal(picker) }
}
