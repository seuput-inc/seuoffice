//! Window handles and the window operations the shell uses beyond Tauri's own API.
//!
//! A [`WindowHandle`] is non-owning access to one exact window: a clone does not keep the
//! window alive and is not an active effect. Each use goes through [`WindowHandle::with_window`],
//! which checks the UI thread, admits one effect on the window's close receipt and hands the
//! raw handle out only inside the closure. HWND and NSWindow values are never stored.

use std::fmt;
use std::sync::mpsc;

use raw_window_handle::HasWindowHandle;
use tauri::{AppHandle, Manager, Window};

use super::Native;
use super::defer::is_ui_thread;
use super::lifecycle::WindowClose;
use super::platform::{Current, WindowOps};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowError {
    /// The raw window handle is only valid on the UI thread.
    NotUiThread,
    /// The window began closing or was replaced.
    Closed,
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUiThread => f.write_str("the native window can only be used on the UI thread"),
            Self::Closed => f.write_str("the original native window was closed or replaced"),
        }
    }
}

impl std::error::Error for WindowError {}

#[derive(Clone)]
pub struct WindowHandle {
    window: Window,
    /// `None` for a window created without a reservation.
    close: Option<WindowClose>,
}

impl fmt::Debug for WindowHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowHandle")
            .field("label", &self.window.label())
            .finish_non_exhaustive()
    }
}

impl WindowHandle {
    pub(crate) fn new(window: Window, close: Option<WindowClose>) -> Self {
        Self { window, close }
    }

    pub fn label(&self) -> &str {
        self.window.label()
    }

    /// Run `effect` on the UI thread with the raw window handle, as one admitted effect.
    pub fn with_window<R>(
        &self,
        effect: impl FnOnce(&dyn HasWindowHandle) -> R,
    ) -> Result<R, WindowError> {
        if !is_ui_thread() {
            return Err(WindowError::NotUiThread);
        }
        let admitted = match &self.close {
            Some(close) => {
                let admitted = close.admit_effect().ok_or(WindowError::Closed)?;
                if !admitted.is_current() {
                    return Err(WindowError::Closed);
                }
                Some(admitted)
            }
            None => None,
        };
        let result = effect(&self.window);
        drop(admitted);
        Ok(result)
    }
}

/// Run `job` on the UI thread and wait for its result (inline when already there).
fn on_ui<T: Send + 'static>(
    window: &Window,
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    if is_ui_thread() {
        return job();
    }
    let (send, receive) = mpsc::sync_channel(1);
    window
        .run_on_main_thread(move || {
            let _ = send.send(job());
        })
        .map_err(|error| error.to_string())?;
    receive
        .recv()
        .map_err(|_| "The UI thread dropped the window operation.".to_owned())?
}

/// Window operations the shell keeps by their old names.
pub trait WindowExt {
    /// Show the window without activating it.
    fn show_inactive(&self) -> Result<(), String>;
    /// Attach this window as a modal child of `owner`.
    fn begin_modal(&self, owner: &Window) -> Result<(), String>;
    /// End this window's modal relationship.
    fn end_modal(&self) -> Result<(), String>;
    /// Non-owning access to this exact window; use `with_window` for each use.
    fn native_window_handle(&self) -> Result<WindowHandle, String>;
}

impl WindowExt for Window {
    fn show_inactive(&self) -> Result<(), String> {
        let window = self.clone();
        on_ui(self, move || Current::show_inactive(&window))
    }

    fn begin_modal(&self, owner: &Window) -> Result<(), String> {
        let (child, owner) = (self.clone(), owner.clone());
        on_ui(self, move || Current::begin_modal(&child, &owner))
    }

    fn end_modal(&self) -> Result<(), String> {
        let window = self.clone();
        on_ui(self, move || Current::end_modal(&window))
    }

    fn native_window_handle(&self) -> Result<WindowHandle, String> {
        let native = self
            .try_state::<Native>()
            .ok_or("The native module is not installed.")?;
        Ok(native.window_handle(self))
    }
}

/// The window AppKit considers key, as an exact reservation. Call on the UI thread before
/// starting a modal loop. `None` when no application window is key.
pub fn native_app_key_window_handle(app: &AppHandle) -> Result<Option<WindowHandle>, String> {
    let Some(label) = Current::key_window_label(app)? else {
        return Ok(None);
    };
    let native = app
        .try_state::<Native>()
        .ok_or("The native module is not installed.")?;
    Ok(app
        .get_window(&label)
        .map(|window| native.window_handle(&window)))
}
