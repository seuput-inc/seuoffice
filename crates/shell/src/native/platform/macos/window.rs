//! `WindowOps` over `NSWindow`. Every call runs on the UI thread, where AppKit objects may be
//! used; the window pointer comes from Tauri and is only dereferenced inside the call.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSWindow};
use tauri::{AppHandle, Manager, Window};

use super::Platform;
use crate::native::platform::WindowOps;

fn ui_thread() -> Result<MainThreadMarker, String> {
    MainThreadMarker::new()
        .ok_or_else(|| "AppKit windows can only be used on the UI thread.".to_owned())
}

/// The `NSWindow` behind a Tauri window, for the duration of the calling job.
fn native(window: &Window) -> Result<&NSWindow, String> {
    let pointer = window.ns_window().map_err(|error| error.to_string())?.cast::<NSWindow>();
    // SAFETY: Tauri returns the live window it owns; the reference is only used on the UI
    // thread inside the caller, which does not outlive the Tauri window handle borrowed here.
    unsafe { pointer.as_ref() }.ok_or_else(|| "The window has no native window yet.".to_owned())
}

impl WindowOps for Platform {
    fn show_inactive(window: &Window) -> Result<(), String> {
        ui_thread()?;
        native(window)?.orderFrontRegardless();
        Ok(())
    }

    fn begin_modal(child: &Window, owner: &Window) -> Result<(), String> {
        ui_thread()?;
        let (child, owner) = (native(child)?, native(owner)?);
        if std::ptr::eq(child, owner) {
            return Err("A window cannot be modal to itself.".to_owned());
        }
        if child.sheetParent().is_some_and(|parent| std::ptr::eq(&*parent, owner)) {
            return Ok(());
        }
        owner.beginSheet_completionHandler(child, None);
        Ok(())
    }

    fn end_modal(child: &Window) -> Result<(), String> {
        ui_thread()?;
        let child = native(child)?;
        if let Some(owner) = child.sheetParent() {
            owner.endSheet(child);
        }
        Ok(())
    }

    fn key_window_label(app: &AppHandle) -> Result<Option<String>, String> {
        let mtm = ui_thread()?;
        let Some(key) = NSApplication::sharedApplication(mtm).keyWindow() else {
            return Ok(None);
        };
        for (label, window) in app.windows() {
            let Ok(pointer) = window.ns_window() else { continue };
            if std::ptr::eq(pointer.cast::<NSWindow>(), &*key) {
                return Ok(Some(label));
            }
        }
        Ok(None)
    }
}
