//! Generate's caption on Windows: the system caption, recoloured with DWM. Windows 11
//! (build 22000+) takes the caption and text colours; Windows 10 keeps the default caption and
//! logs that once.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{Mutex, Once};

use windows::Win32::Foundation::{COLORREF, E_INVALIDARG, HWND};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWINDOWATTRIBUTE, DwmSetWindowAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::Result as WindowsResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptionStyle {
    pub background: [u8; 3],
    pub symbols: [u8; 3],
    /// In device-independent pixels; Generate's caption is 44.
    pub height: u32,
}

/// Windows that `configure` accepted, so `update` can refuse a window it never configured.
static CONFIGURED: Mutex<Option<HashSet<usize>>> = Mutex::new(None);
static WINDOWS_10: Once = Once::new();

fn colorref([red, green, blue]: [u8; 3]) -> COLORREF {
    COLORREF(u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16))
}

fn set_color(
    hwnd: HWND, attribute: DWMWINDOWATTRIBUTE, color: [u8; 3],
) -> WindowsResult<()> {
    let color = colorref(color);
    // SAFETY: `color` outlives the call and the size is its own.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attribute,
            (&raw const color).cast::<c_void>(),
            size_of::<COLORREF>() as u32,
        )
    }
}

/// Paint the caption of the window whose handle value is `native`. Callable from any thread;
/// DWM applies the attributes to the window it names.
pub fn configure(native: usize, style: CaptionStyle) -> Result<(), String> {
    if style.height != 44 {
        return Err("Generate caption height must be 44 DIP".into());
    }
    let hwnd = HWND(native as *mut c_void);
    // SAFETY: `IsWindow` accepts any handle value.
    if native == 0 || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err("The Generate window no longer exists.".into());
    }
    let painted = set_color(hwnd, DWMWA_CAPTION_COLOR, style.background)
        .and_then(|()| set_color(hwnd, DWMWA_TEXT_COLOR, style.symbols));
    match painted {
        Ok(()) => {}
        // Builds before 22000 do not know the attributes and refuse them as invalid.
        Err(error) if error.code() == E_INVALIDARG => {
            WINDOWS_10.call_once(|| {
                eprintln!("shell: this Windows build keeps the default Generate caption");
            });
        }
        Err(error) => return Err(format!("The Generate caption could not be painted: {error}")),
    }
    CONFIGURED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(native);
    Ok(())
}

/// Repaint a window `configure` accepted, for example after a theme change.
pub fn update(native: usize, style: CaptionStyle) -> Result<(), String> {
    let configured = CONFIGURED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .is_some_and(|windows| windows.contains(&native));
    if !configured {
        return Err("native caption is not configured".into());
    }
    configure(native, style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_packed_as_colorref() {
        assert_eq!(colorref([0x24, 0x24, 0x24]), COLORREF(0x0024_2424));
        assert_eq!(colorref([0xff, 0x00, 0x80]), COLORREF(0x0080_00ff));
    }

    #[test]
    fn a_window_that_was_never_configured_is_refused() {
        let style = CaptionStyle { background: [30, 30, 30], symbols: [228, 228, 228], height: 44 };
        assert!(update(0x7fff_0001, style).is_err());
        let tall = CaptionStyle { height: 48, ..style };
        assert!(configure(0x7fff_0001, tall).is_err());
    }
}
