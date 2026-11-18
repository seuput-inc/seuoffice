// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use tauri::{Manager, Runtime, Window};

type CaptureCallback = Box<dyn FnOnce(Result<crate::NativeWindowHandle, String>) + Send>;
struct NativeHandleCompletion(Option<CaptureCallback>);
impl NativeHandleCompletion {
  fn finish(&mut self, result: Result<crate::NativeWindowHandle, String>) {
    if let Some(callback) = self.0.take() {
      crate::native_evaluation::defer_completion(Box::new(move || callback(result)));
    }
  }
}
impl Drop for NativeHandleCompletion {
  fn drop(&mut self) { self.finish(Err("Original native window capture was cancelled.".into())); }
}

pub trait AsCefWindowDispatcher {
  fn as_cef_window_dispatcher(&self) -> Option<&crate::CefWindowDispatcher<tauri::EventLoopMessage>>;
}
impl AsCefWindowDispatcher for crate::CefWindowDispatcher<tauri::EventLoopMessage> {
  fn as_cef_window_dispatcher(&self) -> Option<&crate::CefWindowDispatcher<tauri::EventLoopMessage>> { Some(self) }
}
impl AsCefWindowDispatcher for tauri_runtime::dynamic::DynWindowDispatcher<tauri::EventLoopMessage> {
  fn as_cef_window_dispatcher(&self) -> Option<&crate::CefWindowDispatcher<tauri::EventLoopMessage>> { self.downcast_ref() }
}

#[cfg(target_os = "macos")]
pub trait AsCefRuntimeHandle {
  fn as_cef_runtime_handle(&self) -> Option<&crate::CefRuntimeHandle<tauri::EventLoopMessage>>;
}
#[cfg(target_os = "macos")]
impl AsCefRuntimeHandle for crate::CefRuntimeHandle<tauri::EventLoopMessage> {
  fn as_cef_runtime_handle(&self) -> Option<&crate::CefRuntimeHandle<tauri::EventLoopMessage>> { Some(self) }
}
#[cfg(target_os = "macos")]
impl AsCefRuntimeHandle for tauri_runtime::dynamic::DynRuntimeHandle<tauri::EventLoopMessage> {
  fn as_cef_runtime_handle(&self) -> Option<&crate::CefRuntimeHandle<tauri::EventLoopMessage>> { self.downcast_ref() }
}

/// Capture the real AppKit key window from this application's runtime registry.
/// Call on the UI thread before entering a native modal loop.
#[cfg(target_os = "macos")]
pub fn native_app_key_window_handle<R: Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<Option<crate::NativeWindowHandle>>
where R::Handle: AsCefRuntimeHandle {
  app.runtime_handle().as_cef_runtime_handle()
    .ok_or_else(|| tauri::Error::from(tauri_runtime::Error::RuntimeTypeMismatch("not a CEF runtime handle".into())))?
    .native_key_window_on_ui().map_err(Into::into)
}

/// Native operations on an existing CEF window, including bare windows.
pub trait WindowCefExt<R: Runtime>
where R::WindowDispatcher: AsCefWindowDispatcher {
  /// Show the native window without activating it.
  fn show_inactive(&self) -> tauri::Result<()>;
  /// Attach this window as a modal child of `owner`.
  fn begin_modal(&self, owner: &Window<R>) -> tauri::Result<()>;
  /// End this window's modal relationship.
  fn end_modal(&self) -> tauri::Result<()>;
  /// Capture non-owning access to this exact native window on the UI thread.
  /// A retained handle is not an active effect; use `with_window` for each use.
  fn native_window_handle(&self) -> tauri::Result<crate::NativeWindowHandle>;
  /// Capture the actual AppKit key window as an exact live runtime reservation.
  /// Must be called on the native UI thread before starting the modal loop.
  #[cfg(target_os = "macos")]
  fn native_key_window_handle(&self) -> tauri::Result<Option<crate::NativeWindowHandle>>;
  /// Queue capture on the native UI thread without blocking the caller.
  fn native_window_handle_async<F>(&self, completion: F) -> tauri::Result<()>
  where F: FnOnce(Result<crate::NativeWindowHandle, String>) + Send + 'static;
  /// Query the actual AppKit mode inline on UI, or dispatch there from a worker.
  fn is_simple_fullscreen(&self) -> tauri::Result<bool>
  where R::WindowDispatcher: AsCefWindowDispatcher;
}

impl<R: Runtime> WindowCefExt<R> for Window<R>
where R::WindowDispatcher: AsCefWindowDispatcher {
  fn show_inactive(&self) -> tauri::Result<()> {
    let window = self.clone();
    self.run_on_main_thread_blocking(move || show_native(&window))?
  }

  fn begin_modal(&self, owner: &Window<R>) -> tauri::Result<()> {
    crate::modal_window::begin(self, owner)
  }

  fn end_modal(&self) -> tauri::Result<()> {
    crate::modal_window::end(self)
  }

  fn native_window_handle(&self) -> tauri::Result<crate::NativeWindowHandle> {
    Ok(native_window_on_ui(self)?.handle())
  }

  #[cfg(target_os = "macos")]
  fn native_key_window_handle(&self) -> tauri::Result<Option<crate::NativeWindowHandle>> {
    let dispatcher = self.dispatcher().as_cef_window_dispatcher()
      .ok_or_else(|| tauri::Error::from(tauri_runtime::Error::RuntimeTypeMismatch("not a CEF window dispatcher".into())))?;
    let _original = dispatcher.native_window_on_ui()?;
    dispatcher.context.native_windows.key_window_on_ui().map_err(Into::into)
  }

  fn native_window_handle_async<F>(&self, completion: F) -> tauri::Result<()>
  where F: FnOnce(Result<crate::NativeWindowHandle, String>) + Send + 'static {
    let dispatcher = self.dispatcher().as_cef_window_dispatcher()
      .ok_or_else(|| tauri::Error::from(tauri_runtime::Error::RuntimeTypeMismatch("not a CEF window dispatcher".into())))?
      .clone();
    let (window_id, close) = dispatcher.context.window_scope.clone()
      .ok_or_else(|| tauri::Error::from(tauri_runtime::Error::FailedToSendMessage))?;
    let effect = close.admit_effect()
      .ok_or_else(|| tauri::Error::from(tauri_runtime::Error::FailedToSendMessage))?;
    let windows = dispatcher.context.native_windows.clone();
    let target = dispatcher.clone();
    let mut completion = NativeHandleCompletion(Some(Box::new(completion)));
    dispatcher.context.enqueue_message(crate::runtime::Message::Task(Box::new(move || {
      let _effect = effect;
      let result = if close.is_open() && windows.matches(window_id, &close) {
        target.native_window_on_ui().map(|lease| lease.handle()).map_err(|error| error.to_string())
      } else { Err("Original native window was closed or replaced.".into()) };
      completion.finish(result);
    }))).map_err(Into::into)
  }

  fn is_simple_fullscreen(&self) -> tauri::Result<bool>
  where R::WindowDispatcher: AsCefWindowDispatcher {
    self.dispatcher().as_cef_window_dispatcher()
      .ok_or_else(|| tauri::Error::from(tauri_runtime::Error::RuntimeTypeMismatch("not a CEF window dispatcher".into())))?
      .is_simple_fullscreen().map_err(Into::into)
  }
}

pub(crate) fn native_window_on_ui<R: Runtime>(window: &Window<R>) -> tauri::Result<crate::window::NativeWindowLease>
where R::WindowDispatcher: AsCefWindowDispatcher {
  window.dispatcher().as_cef_window_dispatcher()
    .ok_or_else(|| tauri::Error::from(tauri_runtime::Error::RuntimeTypeMismatch("not a CEF window dispatcher".into())))?
    .native_window_on_ui().map_err(Into::into)
}

#[cfg(target_os = "macos")]
fn show_native<R: Runtime>(window: &Window<R>) -> tauri::Result<()>
where R::WindowDispatcher: AsCefWindowDispatcher {
  let native = native_window_on_ui(window)?;
  let window = crate::platform::macos::nswindow(native.as_ref());
  window.orderFrontRegardless();
  Ok(())
}

#[cfg(windows)]
fn show_native<R: Runtime>(window: &Window<R>) -> tauri::Result<()>
where R::WindowDispatcher: AsCefWindowDispatcher {
  use raw_window_handle::{HasWindowHandle, RawWindowHandle};
  use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE}};
  let native = native_window_on_ui(window)?;
  let handle = native.window_handle().map_err(|_| tauri::Error::InvalidWindowHandle)?;
  let RawWindowHandle::Win32(handle) = handle.as_raw() else { return Err(tauri::Error::InvalidWindowHandle) };
  let hwnd = HWND(handle.hwnd.get() as *mut _);
  // SAFETY: Tauri supplies the live HWND on its owning UI thread.
  unsafe { let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
  Ok(())
}

#[cfg(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
fn show_native<R: Runtime>(window: &Window<R>) -> tauri::Result<()>
where R::WindowDispatcher: AsCefWindowDispatcher {
  crate::platform::linux::show_inactive(window)
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd")))]
fn show_native<R: Runtime>(_window: &Window<R>) -> tauri::Result<()>
where R::WindowDispatcher: AsCefWindowDispatcher {
  Err(tauri_runtime::Error::RuntimeTypeMismatch("nonactivating native window display is unavailable on this platform".into()).into())
}
