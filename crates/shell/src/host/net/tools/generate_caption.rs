//! Generate's native caption. Its caller already runs on the window UI thread.
use main_core::shell::{generate::generate_window::GenerateWindowOptions, title_bar_overlay::TitleBarOverlay};

pub(super) fn configure(
    window: &tauri::Window,
    handle: &crate::native::WindowHandle,
    permit: &crate::native::WindowEffect,
    options: &GenerateWindowOptions,
) -> Result<(), String> {
    handle.with_window(|_| {
        if !permit.is_current() { return Err("The original Generate window is no longer available.".into()); }
        configure_live(window, options)
    }).map_err(|error| error.to_string())?
}

fn configure_live(window: &tauri::Window, options: &GenerateWindowOptions) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = window;
        if options.title_bar_style.is_some() || options.title_bar_overlay.is_some() {
            return Err("Generate on macOS requires its original native frame".into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        if options.title_bar_style != Some("hidden") { return Err("Generate requires the hidden native caption overlay".into()); }
        let overlay = options.title_bar_overlay.as_ref().ok_or("Generate caption overlay is missing")?;
        let style = style(overlay)?;
        crate::native::platform::windows::caption::configure(native_handle(window)?, style)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (window, options);
        Err("native Generate caption is unavailable on this platform".into())
    }
}

pub(super) fn update(
    window: &tauri::Window,
    handle: &crate::native::WindowHandle,
    permit: &crate::native::WindowEffect,
    overlay: &TitleBarOverlay,
) -> Result<(), String> {
    handle.with_window(|_| {
        if !permit.is_current() { return Err("The original Generate window is no longer available.".into()); }
        update_live(window, overlay)
    }).map_err(|error| error.to_string())?
}

fn update_live(window: &tauri::Window, overlay: &TitleBarOverlay) -> Result<(), String> {
    #[cfg(windows)]
    {
        let style = style(overlay)?;
        crate::native::platform::windows::caption::update(native_handle(window)?, style)
    }
    #[cfg(not(windows))]
    {
        let _ = (window, overlay);
        Err("this platform does not use a Generate caption overlay".into())
    }
}

#[cfg(windows)]
fn style(overlay: &TitleBarOverlay) -> Result<crate::native::platform::windows::caption::CaptionStyle, String> {
    if overlay.height.0 != 44.0 { return Err("Generate caption height must be 44 DIP".into()); }
    Ok(crate::native::platform::windows::caption::CaptionStyle {
        background: color(overlay.color)?, symbols: color(overlay.symbol_color)?, height: 44,
    })
}

#[cfg_attr(not(windows), allow(dead_code))]
fn color(value: &str) -> Result<[u8;3], String> {
    let bytes = value.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' { return Err("invalid Generate caption color".into()); }
    fn digit(byte: u8) -> Result<u8,String> {
        match byte { b'0'..=b'9' => Ok(byte-b'0'), b'a'..=b'f' => Ok(byte-b'a'+10), b'A'..=b'F' => Ok(byte-b'A'+10), _ => Err("invalid Generate caption color".into()) }
    }
    Ok([digit(bytes[1])?*16+digit(bytes[2])?, digit(bytes[3])?*16+digit(bytes[4])?, digit(bytes[5])?*16+digit(bytes[6])?])
}

#[cfg(windows)]
fn native_handle(window: &tauri::Window) -> Result<usize,String> {
    window.hwnd().map(|handle| handle.0 as usize).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_light_and_dark_palettes_are_exact() {
        assert_eq!(color("#ffffff").unwrap(), [255,255,255]);
        assert_eq!(color("#242424").unwrap(), [36,36,36]);
        assert_eq!(color("#1e1e1e").unwrap(), [30,30,30]);
        assert_eq!(color("#e4e4e4").unwrap(), [228,228,228]);
        assert!(color("#fff").is_err());
        assert!(color("#gggggg").is_err());
    }
}
