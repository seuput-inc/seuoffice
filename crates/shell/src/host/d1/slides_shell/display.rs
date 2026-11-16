use super::Display;

#[derive(Clone, Copy)]
pub(super) struct NativeDisplay {
    pub bounds: Display,
    pub work: Display,
}

#[cfg(target_os = "macos")]
fn screen_id(screen: &objc2_app_kit::NSScreen) -> Result<i64, String> {
    use objc2_foundation::{NSNumber, ns_string};
    let number = screen.deviceDescription().objectForKey(ns_string!("NSScreenNumber"))
        .ok_or("The native display identifier is unavailable")?
        .downcast::<NSNumber>().map_err(|_| "The native display identifier is invalid")?;
    Ok(i64::from(number.unsignedIntValue()))
}

#[cfg(target_os = "macos")]
fn visit_screens(mut visit: impl FnMut(&objc2_app_kit::NSScreen, i64, f64) -> Result<bool, String>) -> Result<(), String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;
    let mtm = MainThreadMarker::new().ok_or("Native displays must be queried on the main thread")?;
    let screens = NSScreen::screens(mtm);
    let first = screens.firstObject().ok_or("The native display is unavailable")?;
    let primary_frame = first.frame();
    let primary_top = primary_frame.origin.y + primary_frame.size.height;
    for index in 0..screens.len() {
        let screen = screens.objectAtIndex(index);
        if !visit(&screen, screen_id(&screen)?, primary_top)? { break; }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn screen_display(screen: &objc2_app_kit::NSScreen, id: i64, primary_top: f64) -> NativeDisplay {
    let frame = screen.frame();
    let visible = screen.visibleFrame();
    let bounds = Display { id, x: frame.origin.x, y: primary_top - frame.origin.y - frame.size.height,
        width: frame.size.width, height: frame.size.height };
    let work = Display { id, x: visible.origin.x, y: primary_top - visible.origin.y - visible.size.height,
        width: visible.size.width, height: visible.size.height };
    NativeDisplay { bounds, work }
}

#[cfg(target_os = "macos")]
pub(super) fn all() -> Result<Vec<NativeDisplay>, String> {
    let mut displays = Vec::new();
    visit_screens(|screen, id, top| { displays.push(screen_display(screen, id, top)); Ok(true) })?;
    Ok(displays)
}

#[cfg(target_os = "macos")]
pub(super) fn by_id(id: i64) -> Result<NativeDisplay, String> {
    let mut found = None;
    visit_screens(|screen, actual, top| {
        if actual == id { found = Some(screen_display(screen, actual, top)); return Ok(false); }
        Ok(true)
    })?;
    found.ok_or_else(|| "The presentation display was disconnected".into())
}

#[cfg(windows)]
fn enumerate(target: Option<i64>) -> Result<(Vec<NativeDisplay>, Option<NativeDisplay>), String> {
    use windows::Win32::Foundation::{LPARAM, RECT, TRUE, FALSE};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
    use windows::core::BOOL;
    struct State {
        target: Option<i64>,
        displays: Vec<NativeDisplay>,
        found: Option<NativeDisplay>,
        error: Option<String>,
    }
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: EnumDisplayMonitors synchronously passes the live, exclusive State pointer.
        let state = unsafe { &mut *(data.0 as *mut State) };
        let id = monitor.0 as isize as i64;
        if state.target.is_some_and(|target| target != id) { return TRUE; }
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        // SAFETY: monitor is supplied by the OS and info is initialized with its exact size.
        if let Err(error) = unsafe { GetMonitorInfoW(monitor, &mut info) }.ok() {
            state.error = Some(error.to_string());
            return FALSE;
        }
        let rect = |r: RECT| Display { id, x: f64::from(r.left), y: f64::from(r.top),
            width: f64::from(r.right - r.left), height: f64::from(r.bottom - r.top) };
        let display = NativeDisplay { bounds: rect(info.rcMonitor), work: rect(info.rcWork) };
        if state.target.is_some() { state.found = Some(display); FALSE }
        else { state.displays.push(display); TRUE }
    }
    let mut state = State { target, displays: Vec::new(), found: None, error: None };
    // SAFETY: the callback completes synchronously and only accesses state during this call.
    let result = unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM((&mut state as *mut State) as isize)) };
    if let Some(error) = state.error { return Err(error); }
    if state.found.is_none() { result.ok().map_err(|error| error.to_string())?; }
    Ok((state.displays, state.found))
}

#[cfg(windows)]
pub(super) fn all() -> Result<Vec<NativeDisplay>, String> {
    enumerate(None).map(|(displays, _)| displays)
}

#[cfg(windows)]
pub(super) fn by_id(id: i64) -> Result<NativeDisplay, String> {
    enumerate(Some(id))?.1.ok_or_else(|| "The presentation display was disconnected".into())
}

#[cfg(target_os = "macos")]
pub(super) fn for_window(window: &tauri::Window) -> Result<Display, String> {
    use objc2::rc::Retained;
    use objc2_app_kit::NSWindow;
    let pointer = window.ns_window().map_err(|error| error.to_string())?;
    // SAFETY: the caller holds the original window's UI-thread native effect guard.
    let window = unsafe { Retained::retain(pointer.cast::<NSWindow>()) }
        .ok_or("The native presentation window is unavailable")?;
    let screen = window.screen().ok_or("The native display is unavailable")?;
    Ok(by_id(screen_id(&screen)?)?.bounds)
}

#[cfg(windows)]
pub(super) fn for_window(window: &tauri::Window) -> Result<Display, String> {
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    // SAFETY: the caller holds the original window's UI-thread native effect guard.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_invalid() { return Err("The native display is unavailable".into()); }
    Ok(by_id(monitor.0 as isize as i64)?.bounds)
}

#[cfg(target_os = "macos")]
pub(super) fn primary() -> Result<Display, String> {
    let mut first = None;
    visit_screens(|screen, id, top| { first = Some(screen_display(screen, id, top).bounds); Ok(false) })?;
    first.ok_or_else(|| "The native display is unavailable".into())
}

#[cfg(windows)]
pub(super) fn primary() -> Result<Display, String> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
    // SAFETY: the point and selection flag are valid values for MonitorFromPoint.
    let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    if monitor.is_invalid() { return Err("The native display is unavailable".into()); }
    Ok(by_id(monitor.0 as isize as i64)?.bounds)
}
