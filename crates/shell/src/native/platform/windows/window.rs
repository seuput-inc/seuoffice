use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use tauri::{AppHandle, Manager, Window};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SW_SHOWNOACTIVATE, ShowWindow,
};

use super::Platform;
use crate::native::platform::WindowOps;

/// An owner kept disabled by its modal children, with the state to restore.
struct Owner {
    was_enabled: bool,
    children: Vec<usize>,
}

#[derive(Default)]
struct Modals {
    owners: HashMap<usize, Owner>,
    /// Child window to the owner it blocks.
    blocking: HashMap<usize, usize>,
}

thread_local! {
    static MODALS: RefCell<Modals> = RefCell::new(Modals::default());
}

fn hwnd_of(window: &Window) -> Result<HWND, String> {
    let raw = window.hwnd().map_err(|error| error.to_string())?;
    // Tauri's `HWND` may come from another `windows` version; only the value crosses over.
    Ok(from_key(raw.0 as usize))
}

fn from_key(key: usize) -> HWND {
    HWND(key as *mut c_void)
}

fn key(hwnd: HWND) -> usize {
    hwnd.0 as usize
}

fn exists(hwnd: HWND) -> bool {
    // SAFETY: `IsWindow` accepts any handle value.
    unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

fn set_enabled(hwnd: HWND, enabled: bool) {
    // SAFETY: UI thread of the window; the call only changes its enabled state.
    unsafe { EnableWindow(hwnd, enabled) };
}

impl WindowOps for Platform {
    fn show_inactive(window: &Window) -> Result<(), String> {
        let hwnd = hwnd_of(window)?;
        if !exists(hwnd) {
            return Err("The window no longer exists.".into());
        }
        // SAFETY: a live window of this thread; the return value is its previous visibility.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        Ok(())
    }

    /// Disables `owner` (remembering whether it was enabled) until its last modal child ends.
    fn begin_modal(child: &Window, owner: &Window) -> Result<(), String> {
        let (child, owner) = (hwnd_of(child)?, hwnd_of(owner)?);
        if child == owner {
            return Err("A window cannot be modal to itself.".into());
        }
        if !exists(child) || !exists(owner) {
            return Err("The modal window or its owner no longer exists.".into());
        }
        MODALS.with(|modals| {
            let mut modals = modals.borrow_mut();
            if modals.blocking.contains_key(&key(child)) {
                return Err("The window is already modal.".to_owned());
            }
            let entry = modals.owners.entry(key(owner)).or_insert_with(|| {
                // SAFETY: a live window of this thread.
                let was_enabled = unsafe { IsWindowEnabled(owner) }.as_bool();
                set_enabled(owner, false);
                Owner {
                    was_enabled,
                    children: Vec::new(),
                }
            });
            entry.children.push(key(child));
            modals.blocking.insert(key(child), key(owner));
            Ok(())
        })
    }

    fn end_modal(child: &Window) -> Result<(), String> {
        let child = key(hwnd_of(child)?);
        MODALS.with(|modals| {
            let mut modals = modals.borrow_mut();
            let Some(owner) = modals.blocking.remove(&child) else {
                return Ok(());
            };
            let Some(entry) = modals.owners.get_mut(&owner) else {
                return Ok(());
            };
            // A child destroyed without ending its modality must not keep the owner disabled.
            entry
                .children
                .retain(|other| *other != child && exists(from_key(*other)));
            if !entry.children.is_empty() {
                return Ok(());
            }
            let was_enabled = entry.was_enabled;
            modals.owners.remove(&owner);
            let owner = from_key(owner);
            if was_enabled && exists(owner) {
                set_enabled(owner, true);
            }
            Ok(())
        })
    }

    /// The foreground window when it belongs to this process and is one of the app's windows.
    fn key_window_label(app: &AppHandle) -> Result<Option<String>, String> {
        // SAFETY: both calls read system state only.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.is_invalid() {
            return Ok(None);
        }
        let mut process = 0u32;
        // SAFETY: `process` outlives the call.
        unsafe { GetWindowThreadProcessId(foreground, Some(&mut process)) };
        // SAFETY: reads this process's id.
        if process != unsafe { GetCurrentProcessId() } {
            return Ok(None);
        }
        let label = app
            .windows()
            .into_iter()
            .find(|(_, window)| hwnd_of(window).is_ok_and(|hwnd| hwnd == foreground))
            .map(|(label, _)| label);
        Ok(label)
    }
}
