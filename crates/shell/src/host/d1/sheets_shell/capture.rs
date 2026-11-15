//! OS desktop/window capture. The provider never receives renderer-selected paths.
use std::io::Cursor;

use image::{ImageFormat, RgbaImage};
use shell_contract::{WindowId, ipc::NativeRequestOwner, runtime::CancelToken};
use shell_ipc_d1::sheets::engine::{ScreenCapture, ScreenImage, ScreenSource};
use tauri::Manager;

use crate::{host::{HostContext, on_ui}, ipc::native_owner};

pub(super) struct Capture {
    cx: HostContext,
    #[cfg(target_os = "linux")]
    portal: super::portal::Selections,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceId { Screen(u32), Window(u32) }

impl SourceId {
    fn parse(value: &str) -> Result<Self, String> {
        let mut parts = value.split(':');
        let kind = parts.next();
        let id = parts.next().ok_or("Invalid screen source identifier")?;
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit())
            || (id.len() > 1 && id.starts_with('0'))
            || parts.next() != Some("0") || parts.next().is_some()
        { return Err("Invalid screen source identifier".into()); }
        let id = id.parse::<u32>().map_err(|_| "Invalid screen source identifier")?;
        match kind {
            Some("screen") => Ok(Self::Screen(id)),
            Some("window") => Ok(Self::Window(id)),
            _ => Err("Invalid screen source identifier".into()),
        }
    }
    fn wire(self) -> String {
        match self { Self::Screen(id) => format!("screen:{id}:0"), Self::Window(id) => format!("window:{id}:0") }
    }
}

pub(super) fn admitted<T>(owner: &NativeRequestOwner, cancel: &CancelToken, work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let accepted = native_owner::with_owner_effect(owner, cancel, || work)
        .ok_or("The screen capture request is no longer active")?;
    accepted()
}

pub(super) fn deliver<T>(owner: &NativeRequestOwner, cancel: &CancelToken, value: T) -> Result<T, String> {
    native_owner::with_owner_effect(owner, cancel, || value)
        .ok_or_else(|| "The screen capture request is no longer active".into())
}

fn png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png).map_err(|error| error.to_string())?;
    Ok(output.into_inner())
}

fn fit(image: RgbaImage, width: u32, height: u32) -> RgbaImage {
    if image.width() <= width && image.height() <= height { image }
    else { image::imageops::thumbnail(&image, width, height) }
}

pub(super) fn thumbnail(image: RgbaImage) -> Result<String, String> {
    if image.width() == 0 || image.height() == 0 { return Ok(String::new()); }
    let image = fit(image, 320, 200);
    Ok(format!("data:image/png;base64,{}", jscompat::base64(&png(&image)?)))
}

fn capture_size(displays: impl IntoIterator<Item = (u32, u32)>) -> (u32, u32) {
    let (width, height) = displays.into_iter().fold((1920, 1080), |(width, height), (w, h)| (width.max(w), height.max(h)));
    (width.min(4096), height.min(4096))
}

pub(super) fn encoded_capture(image: RgbaImage, size: (u32, u32)) -> Result<Option<ScreenImage>, String> {
    if image.width() == 0 || image.height() == 0 { return Ok(None); }
    let mut image = fit(image, size.0, size.1);
    let mut bytes = png(&image)?;
    if bytes.len() > 20 * 1024 * 1024 {
        let width = image.width().div_ceil(2).max(1);
        let height = ((f64::from(image.height()) * f64::from(width) / f64::from(image.width())).round() as u32).max(1);
        image = image::imageops::resize(&image, width, height, image::imageops::FilterType::Lanczos3);
        bytes = png(&image)?;
    }
    Ok(Some(ScreenImage { png: bytes, width: f64::from(image.width()), height: f64::from(image.height()) }))
}

impl Capture {
    pub(super) fn new(cx: &HostContext) -> Self {
        Self { cx: cx.clone(), #[cfg(target_os = "linux")] portal: Default::default() }
    }

    #[cfg(target_os = "linux")]
    fn portal_capture_size(&self, owner: &NativeRequestOwner, cancel: &CancelToken) -> Result<(u32, u32), String> {
        let cx = self.cx.clone();
        let owner = owner.clone();
        let cancel = cancel.clone();
        on_ui(&self.cx.ui, move || native_owner::run_accepted_ui(cx.ui.as_ref(), &owner, &cancel, || {
            let monitors = cx.app.available_monitors().map_err(|error| error.to_string())?;
            Ok(capture_size(monitors.into_iter().map(|monitor| (monitor.size().width, monitor.size().height))))
        }).and_then(|result| result))
    }

    fn excluded_window(&self, owner: &NativeRequestOwner, cancel: &CancelToken, exclude: Option<WindowId>) -> Result<Option<u32>, String> {
        let Some(exclude) = exclude else { return Ok(None); };
        let lifecycle = self.cx.views.window_lifecycle(exclude).ok_or("The capturing window is no longer available")?;
        let window = lifecycle.window_handle()?;
        let cx = self.cx.clone();
        let owner = owner.clone();
        let cancel = cancel.clone();
        on_ui(&self.cx.ui, move || native_owner::run_accepted_ui(cx.ui.as_ref(), &owner, &cancel, || {
            use tauri_runtime_cef::WindowCefExt;
            let handle = window.native_window_handle().map_err(|error| error.to_string())?;
            handle.with_window(|_| native_window_id(&window)).map_err(|error| error.to_string())?
        }).and_then(|result| result))
    }
}

impl ScreenCapture for Capture {
    fn media_access_status(&self, owner: &NativeRequestOwner, cancel: &CancelToken) -> Result<String, String> {
        let status = admitted(owner, cancel, permission_status)?;
        deliver(owner, cancel, status)
    }

    fn list_sources(&self, owner: &NativeRequestOwner, cancel: &CancelToken, exclude: Option<WindowId>) -> Result<Vec<ScreenSource>, String> {
        #[cfg(target_os = "linux")]
        if super::portal::enabled() { return self.portal.list(owner, cancel); }
        let excluded = self.excluded_window(owner, cancel, exclude)?;
        let monitors = admitted(owner, cancel, || xcap::Monitor::all().map_err(|error| error.to_string()))?;
        let mut sources = Vec::new();
        for monitor in monitors {
            let id = monitor.id().map_err(|error| error.to_string())?;
            let name = monitor.friendly_name().map_err(|error| error.to_string())?;
            let image = admitted(owner, cancel, || monitor.capture_image().map_err(|error| error.to_string()))?;
            sources.push(ScreenSource { id: SourceId::Screen(id).wire(), name, thumbnail: thumbnail(image)? });
        }
        let windows = admitted(owner, cancel, || xcap::Window::all().map_err(|error| error.to_string()))?;
        for window in windows {
            let id = window.id().map_err(|error| error.to_string())?;
            if excluded == Some(id) { continue; }
            let name = window.title().map_err(|error| error.to_string())?;
            let image = admitted(owner, cancel, || window.capture_image().map_err(|error| error.to_string()))?;
            sources.push(ScreenSource { id: SourceId::Window(id).wire(), name, thumbnail: thumbnail(image)? });
        }
        deliver(owner, cancel, sources)
    }

    fn capture(&self, owner: &NativeRequestOwner, cancel: &CancelToken, id: &str) -> Result<Option<ScreenImage>, String> {
        #[cfg(target_os = "linux")]
        if super::portal::enabled() { return self.portal.capture(owner, cancel, id, self.portal_capture_size(owner, cancel)?); }
        let Ok(id) = SourceId::parse(id) else { return deliver(owner, cancel, None); };
        let monitors = admitted(owner, cancel, || xcap::Monitor::all().map_err(|error| error.to_string()))?;
        let mut dimensions = Vec::with_capacity(monitors.len());
        for monitor in &monitors {
            dimensions.push((monitor.width().map_err(|error| error.to_string())?, monitor.height().map_err(|error| error.to_string())?));
        }
        let size = capture_size(dimensions);
        let image = match id {
            SourceId::Screen(id) => {
                let mut found = None;
                for monitor in monitors {
                    if monitor.id().map_err(|error| error.to_string())? == id {
                        found = Some(admitted(owner, cancel, || monitor.capture_image().map_err(|error| error.to_string()))?);
                        break;
                    }
                }
                found
            }
            SourceId::Window(id) => {
                let windows = admitted(owner, cancel, || xcap::Window::all().map_err(|error| error.to_string()))?;
                let mut found = None;
                for window in windows {
                    if window.id().map_err(|error| error.to_string())? == id {
                        found = Some(admitted(owner, cancel, || window.capture_image().map_err(|error| error.to_string()))?);
                        break;
                    }
                }
                found
            }
        };
        let result = image.map(|image| encoded_capture(image, size)).transpose()?.flatten();
        deliver(owner, cancel, result)
    }
}

#[cfg(target_os = "macos")]
fn permission_status() -> Result<String, String> {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" { fn CGPreflightScreenCaptureAccess() -> bool; }
    Ok(if unsafe { CGPreflightScreenCaptureAccess() } { "granted" } else { "denied" }.into())
}
#[cfg(not(target_os = "macos"))]
fn permission_status() -> Result<String, String> { Ok("granted".into()) }

#[cfg(target_os = "macos")]
fn native_window_id(window: &tauri::Window) -> Result<Option<u32>, String> {
    let pointer = window.ns_window().map_err(|error| error.to_string())?;
    let window = unsafe { pointer.cast::<objc2_app_kit::NSWindow>().as_ref() }.ok_or("The native window handle is unavailable")?;
    u32::try_from(window.windowNumber()).map(Some).map_err(|_| "The native window identifier is out of range".into())
}
#[cfg(not(target_os = "macos"))]
fn native_window_id(window: &tauri::Window) -> Result<Option<u32>, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().map_err(|error| error.to_string())?.as_raw() {
        #[cfg(windows)]
        RawWindowHandle::Win32(handle) => Ok(Some(handle.hwnd.get() as u32)),
        #[cfg(target_os = "linux")]
        RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).map(Some).map_err(|_| "The X11 window identifier is out of range".into()),
        #[cfg(target_os = "linux")]
        RawWindowHandle::Xcb(handle) => Ok(Some(handle.window.get())),
        // xcap 0.9.10 enumerates only XCB window IDs, never native Wayland surfaces.
        #[cfg(target_os = "linux")]
        RawWindowHandle::Wayland(_) => Ok(None),
        _ => Err("The capture provider does not recognize this native window handle".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_identity_keeps_screen_and_window_namespaces_separate() {
        assert_eq!(SourceId::parse("screen:17:0"), Ok(SourceId::Screen(17)));
        assert_eq!(SourceId::parse("window:17:0"), Ok(SourceId::Window(17)));
        for invalid in ["window:17", "window:-1:0", "screen:1:0:more", "screen:4294967296:0", "screen:1:1", "screen:01:0"] {
            assert!(SourceId::parse(invalid).is_err());
        }
    }

    #[test]
    fn physical_monitors_set_bounded_capture_resolution() {
        assert_eq!(capture_size([]), (1920, 1080));
        assert_eq!(capture_size([(3840, 2160), (7680, 1080)]), (4096, 2160));
    }

    #[test]
    fn real_png_preserves_pixels_and_aspect_ratio() {
        let image = RgbaImage::from_pixel(640, 400, image::Rgba([17, 41, 83, 255]));
        let result = encoded_capture(image, (320, 200)).unwrap().unwrap();
        let decoded = image::load_from_memory(&result.png).unwrap().to_rgba8();
        assert_eq!((result.width, result.height), (320.0, 200.0));
        assert_eq!(decoded.dimensions(), (320, 200));
        assert_eq!(*decoded.get_pixel(10, 10), image::Rgba([17, 41, 83, 255]));
    }

    #[test]
    fn empty_provider_image_is_the_only_empty_image_result() {
        assert!(encoded_capture(RgbaImage::new(0, 0), (1920, 1080)).unwrap().is_none());
    }
}

#[cfg(test)]
mod large_png_tests {
    use super::*;

    #[test]
    fn an_oversize_real_png_is_halved_once_before_delivery() {
        let mut image = RgbaImage::new(3072, 2048);
        let mut state = 0x7a58_253d_9c31_46bfu64;
        for byte in image.as_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let result = encoded_capture(image, (4096, 4096)).unwrap().unwrap();
        assert_eq!((result.width, result.height), (1536.0, 1024.0));
        assert_eq!(image::load_from_memory(&result.png).unwrap().width(), 1536);
    }
}
