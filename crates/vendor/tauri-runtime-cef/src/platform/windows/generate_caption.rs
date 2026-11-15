//! Native Generate close control and drag routing without changing the CEF viewport.
use std::{cell::{Cell, RefCell}, collections::HashMap, rc::Rc};
use windows::{core::{w, BOOL, PCWSTR}, Win32::{
    Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{CreatePen, CreateSolidBrush, DeleteObject, DrawFocusRect, FillRect, HGDIOBJ, InvalidateRect, LineTo, MoveToEx, PS_SOLID, ScreenToClient, SelectObject},
    System::Threading::GetCurrentThreadId,
    UI::{Controls::{DRAWITEMSTRUCT, ODS_FOCUS, ODS_SELECTED, WM_MOUSELEAVE}, HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::{ReleaseCapture, TrackMouseEvent, TRACKMOUSEEVENT, TME_LEAVE},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass}, WindowsAndMessaging::*},
}};

#[derive(Clone, Copy, Debug)]
pub struct CaptionStyle { pub background: [u8; 3], pub symbols: [u8; 3], pub height: u32 }

struct Caption {
    root: HWND,
    button: Cell<HWND>,
    style: Cell<CaptionStyle>,
    hover: Cell<bool>,
    attaching: Cell<bool>,
    windows: RefCell<Vec<HWND>>,
    failure: RefCell<Option<String>>,
}
thread_local! { static WINDOWS: RefCell<HashMap<usize, Rc<Caption>>> = RefCell::new(HashMap::new()); }
const SUBCLASS: usize = 0x53475543;
const CLOSE_ID: usize = 0x5347;

fn physical(dip: u32, dpi: u32) -> i32 { ((u64::from(dip) * u64::from(dpi) + 48) / 96) as i32 }
fn rgb([r,g,b]: [u8;3]) -> COLORREF { COLORREF(u32::from(r) | (u32::from(g)<<8) | (u32::from(b)<<16)) }
fn failure(context: &str) -> String { format!("{context}: {}", windows::core::Error::from_win32()) }

/// # Safety
/// `native` is a live HWND, borrowed on its owning UI thread for this call.
pub unsafe fn configure(native: usize, style: CaptionStyle) -> Result<(), String> {
    if style.height != 44 { return Err("Generate caption height must be 44 DIP".into()); }
    let root = HWND(native as *mut _);
    if native == 0 || unsafe { GetWindowThreadProcessId(root, None) } != unsafe { GetCurrentThreadId() } {
        return Err("Generate caption must be configured on the window UI thread".into());
    }
    let existing = WINDOWS.with(|windows| windows.borrow().get(&native).cloned());
    if let Some(caption) = existing {
        caption.style.set(style);
        return unsafe { layout(&caption) };
    }
    let caption = Rc::new(Caption { root, button: Cell::new(HWND::default()), style: Cell::new(style),
        hover: Cell::new(false), attaching: Cell::new(false), windows: RefCell::new(Vec::new()), failure: RefCell::new(None) });
    WINDOWS.with(|windows| { windows.borrow_mut().insert(native, caption.clone()); });
    let result = (|| unsafe {
        bind(&caption, root)?;
        let menu = GetSystemMenu(root, false);
        if menu.0.is_null() { return Err("Generate window has no native close action".into()); }
        let mut name = [0u16; 256];
        let count = GetMenuStringW(menu, SC_CLOSE, Some(&mut name), MF_BYCOMMAND);
        if count <= 0 { return Err("native close action has no localized accessible name".into()); }
        let text = String::from_utf16_lossy(&name[..count as usize]);
        let text = text.split('\t').next().unwrap_or("").replace('&', "");
        let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let button = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("BUTTON"), PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_OWNERDRAW as u32), 0, 0, 1, 1,
            Some(root), Some(HMENU(CLOSE_ID as *mut _)), None, None).map_err(|error| error.to_string())?;
        caption.button.set(button);
        bind(&caption, button)?;
        attach_tree(&caption)?;
        layout(&caption)
    })();
    if result.is_err() { unsafe { detach(&caption, true); } }
    result
}

/// # Safety
/// Same live-window and UI-thread requirements as `configure`.
pub unsafe fn update(native: usize, style: CaptionStyle) -> Result<(), String> {
    let installed = WINDOWS.with(|windows| windows.borrow().contains_key(&native));
    if !installed { return Err("native caption is not configured".into()); }
    unsafe { configure(native, style) }
}

unsafe fn bind(caption: &Rc<Caption>, window: HWND) -> Result<(), String> {
    if caption.windows.borrow().contains(&window) { return Ok(()); }
    if unsafe { GetWindowThreadProcessId(window, None) } != unsafe { GetCurrentThreadId() } {
        return Err("Generate CEF child is not on the owning UI thread".into());
    }
    if !unsafe { SetWindowSubclass(window, Some(procedure), SUBCLASS, Rc::as_ptr(caption) as usize) }.as_bool() {
        return Err(failure("cannot install native Generate caption routing"));
    }
    caption.windows.borrow_mut().push(window);
    Ok(())
}

unsafe extern "system" fn enumerate(window: HWND, data: LPARAM) -> BOOL {
    // SAFETY: EnumChildWindows synchronously borrows this Rc from attach_tree.
    let caption = unsafe { &*(data.0 as *const Rc<Caption>) };
    if let Err(error) = unsafe { bind(caption, window) } {
        *caption.failure.borrow_mut() = Some(error);
        return BOOL(0);
    }
    BOOL(1)
}

unsafe fn attach_tree(caption: &Rc<Caption>) -> Result<(), String> {
    if caption.attaching.replace(true) { return Ok(()); }
    caption.failure.borrow_mut().take();
    unsafe { let _ = EnumChildWindows(Some(caption.root), Some(enumerate), LPARAM(caption as *const Rc<Caption> as isize)); }
    caption.attaching.set(false);
    if let Some(error) = caption.failure.borrow_mut().take() { Err(error) } else { Ok(()) }
}

unsafe fn layout(caption: &Rc<Caption>) -> Result<(), String> {
    unsafe { attach_tree(caption)?; }
    let mut bounds = RECT::default();
    unsafe { GetClientRect(caption.root, &mut bounds) }.map_err(|error| error.to_string())?;
    let dpi = unsafe { GetDpiForWindow(caption.root) };
    if dpi == 0 { return Err("native caption window no longer has monitor DPI".into()); }
    let height = physical(caption.style.get().height, dpi).min(bounds.bottom.max(0));
    let width = physical(46, dpi).min(bounds.right.max(0));
    unsafe { SetWindowPos(caption.button.get(), Some(HWND_TOP), bounds.right-width, 0, width, height, SWP_NOACTIVATE) }
        .map_err(|error| error.to_string())?;
    unsafe { let _ = InvalidateRect(Some(caption.button.get()), None, true); }
    Ok(())
}

unsafe fn in_header(caption: &Caption, screen: POINT) -> bool {
    let mut point = screen;
    if !unsafe { ScreenToClient(caption.root, &mut point) }.as_bool() { return false; }
    let mut bounds = RECT::default();
    if unsafe { GetClientRect(caption.root, &mut bounds) }.is_err() { return false; }
    let dpi = unsafe { GetDpiForWindow(caption.root) };
    if dpi == 0 { return false; }
    point.x >= 0 && point.x < bounds.right-physical(46,dpi) && point.y >= 0 && point.y < physical(44,dpi)
}
fn screen_point(value: LPARAM) -> POINT { POINT { x: value.0 as i16 as i32, y: (value.0 >> 16) as i16 as i32 } }

unsafe fn system_menu(caption: &Caption) {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() { return; }
    let menu = unsafe { GetSystemMenu(caption.root, false) };
    let command = unsafe { TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, point.x, point.y, None, caption.root, None) }.0 as usize;
    if command != 0 {
        if let Err(error) = unsafe { PostMessageW(Some(caption.root), WM_SYSCOMMAND, WPARAM(command), LPARAM(0)) } {
            eprintln!("Generate system action: {error}");
        }
    }
}

unsafe fn draw(caption: &Caption, item: &DRAWITEMSTRUCT) {
    let hover = caption.hover.get() || item.itemState.0 & ODS_SELECTED.0 != 0;
    let style = caption.style.get();
    let background = if hover { [232,17,35] } else { style.background };
    let foreground = if hover { [255,255,255] } else { style.symbols };
    let brush = unsafe { CreateSolidBrush(rgb(background)) };
    if !brush.0.is_null() {
        unsafe { FillRect(item.hDC, &item.rcItem, brush); let _ = DeleteObject(HGDIOBJ(brush.0)); }
    }
    let dpi = unsafe { GetDpiForWindow(caption.root) };
    if dpi == 0 { return; }
    let pen = unsafe { CreatePen(PS_SOLID, physical(1,dpi).max(1), rgb(foreground)) };
    if !pen.0.is_null() {
        let old = unsafe { SelectObject(item.hDC, HGDIOBJ(pen.0)) };
        let x = (item.rcItem.left+item.rcItem.right)/2;
        let y = (item.rcItem.top+item.rcItem.bottom)/2;
        let half = physical(5,dpi);
        unsafe {
            let _ = MoveToEx(item.hDC,x-half,y-half,None); let _ = LineTo(item.hDC,x+half,y+half);
            let _ = MoveToEx(item.hDC,x-half,y+half,None); let _ = LineTo(item.hDC,x+half,y-half);
            SelectObject(item.hDC, old); let _ = DeleteObject(HGDIOBJ(pen.0));
        }
    }
    if item.itemState.0 & ODS_FOCUS.0 != 0 { unsafe { let _ = DrawFocusRect(item.hDC, &item.rcItem); } }
}

unsafe fn detach(caption: &Rc<Caption>, destroy_button: bool) {
    let windows = std::mem::take(&mut *caption.windows.borrow_mut());
    for window in windows { unsafe { let _ = RemoveWindowSubclass(window, Some(procedure), SUBCLASS); } }
    WINDOWS.with(|windows| { windows.borrow_mut().remove(&(caption.root.0 as usize)); });
    if destroy_button && !caption.button.get().0.is_null() {
        unsafe { let _ = DestroyWindow(caption.button.replace(HWND::default())); }
    }
}

unsafe extern "system" fn procedure(window: HWND, message: u32, wp: WPARAM, lp: LPARAM, _: usize, data: usize) -> LRESULT {
    // SAFETY: the UI-thread registry keeps this Rc alive until all subclasses are
    // removed. A local strong reference also survives reentrant native teardown.
    unsafe { Rc::increment_strong_count(data as *const Caption); }
    let caption = unsafe { Rc::from_raw(data as *const Caption) };
    if message == WM_NCDESTROY {
        if window == caption.root { unsafe { detach(&caption, false); } }
        else {
            unsafe { let _ = RemoveWindowSubclass(window, Some(procedure), SUBCLASS); }
            caption.windows.borrow_mut().retain(|entry| *entry != window);
        }
        return unsafe { DefSubclassProc(window,message,wp,lp) };
    }
    if window != caption.root && message == WM_NCHITTEST {
        let root_hit = unsafe { SendMessageW(caption.root,WM_NCHITTEST,Some(wp),Some(lp)) };
        if (HTLEFT as isize..=HTBOTTOMRIGHT as isize).contains(&root_hit.0) { return root_hit; }
    }
    if window != caption.root && message == WM_NCLBUTTONDOWN &&
        (wp.0 == HTCAPTION as usize || (HTLEFT as usize..=HTBOTTOMRIGHT as usize).contains(&wp.0)) {
        unsafe { let _ = ReleaseCapture(); }
        return unsafe { SendMessageW(caption.root,WM_NCLBUTTONDOWN,Some(wp),Some(lp)) };
    }
    if window != caption.root && window != caption.button.get() &&
        matches!(message, WM_WINDOWPOSCHANGED | WM_SHOWWINDOW) {
        let result = unsafe { DefSubclassProc(window,message,wp,lp) };
        if !caption.button.get().0.is_null() {
            if let Err(error) = unsafe { SetWindowPos(caption.button.get(),Some(HWND_TOP),0,0,0,0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE) } {
                eprintln!("Generate native close stacking: {error}");
            }
        }
        return result;
    }
    if window == caption.button.get() {
        if message == WM_MOUSEMOVE && !caption.hover.replace(true) {
            let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: window, dwHoverTime: 0 };
            unsafe { let _ = TrackMouseEvent(&mut track); let _ = InvalidateRect(Some(window),None,false); }
        } else if message == WM_MOUSELEAVE {
            caption.hover.set(false);
            unsafe { let _ = InvalidateRect(Some(window),None,false); }
        }
        return unsafe { DefSubclassProc(window,message,wp,lp) };
    }
    if window == caption.root {
        if !caption.button.get().0.is_null() && message == WM_COMMAND && lp.0 == caption.button.get().0 as isize && (wp.0 >> 16) as u32 == BN_CLICKED {
            if let Err(error) = unsafe { PostMessageW(Some(window),WM_CLOSE,WPARAM(0),LPARAM(0)) } { eprintln!("Generate close: {error}"); }
            return LRESULT(0);
        }
        if message == WM_DRAWITEM && lp.0 != 0 {
            let item = unsafe { &*(lp.0 as *const DRAWITEMSTRUCT) };
            if item.hwndItem == caption.button.get() { unsafe { draw(&caption,item); } return LRESULT(1); }
        }
        if !caption.button.get().0.is_null() && matches!(message, WM_SIZE | WM_DPICHANGED | WM_WINDOWPOSCHANGED | WM_PARENTNOTIFY) {
            let result = unsafe { DefSubclassProc(window,message,wp,lp) };
            if let Err(error) = unsafe { layout(&caption) } { eprintln!("Generate caption layout: {error}"); }
            return result;
        }
    }
    if message == WM_NCHITTEST {
        let result = unsafe { DefSubclassProc(window,message,wp,lp) };
        if result.0 == HTCLIENT as isize && unsafe { in_header(&caption,screen_point(lp)) } { return LRESULT(HTCAPTION as isize); }
        return result;
    }
    if message == WM_NCLBUTTONDBLCLK && wp.0 == HTCAPTION as usize { return LRESULT(0); }
    if message == WM_NCRBUTTONUP && wp.0 == HTCAPTION as usize { unsafe { system_menu(&caption); } return LRESULT(0); }
    unsafe { DefSubclassProc(window,message,wp,lp) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caption_dimensions_follow_monitor_dpi_without_changing_browser_geometry() {
        assert_eq!((physical(44,96),physical(46,96)),(44,46));
        assert_eq!((physical(44,144),physical(46,144)),(66,69));
        assert_eq!((physical(44,192),physical(46,192)),(88,92));
    }
    #[test]
    fn signed_desktop_coordinates_survive_native_message_packing() {
        let packed = ((-20i16 as u16 as usize)<<16) | (-100i16 as u16 as usize);
        let point = screen_point(LPARAM(packed as isize));
        assert_eq!((point.x,point.y),(-100,-20));
    }
    #[test]
    #[ignore = "hosted Windows UI thread fixture; never run against user windows"]
    fn native_close_drag_theme_and_teardown_preserve_the_viewport() {
        use windows::{core::Interface, Win32::{System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED},
            UI::Accessibility::{AccessibleObjectFromWindow, IAccessible}}};
        struct Apartment;
        impl Drop for Apartment { fn drop(&mut self) { unsafe { CoUninitialize(); } } }
        struct OwnedWindow(HWND);
        impl Drop for OwnedWindow { fn drop(&mut self) { unsafe { if IsWindow(Some(self.0)).as_bool() { DestroyWindow(self.0).unwrap(); } } } }
        unsafe { CoInitializeEx(None,COINIT_APARTMENTTHREADED).ok().unwrap(); }
        let _apartment = Apartment;
        let root = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(),w!("STATIC"),w!("synthetic Generate"),
            WS_POPUP | WS_SYSMENU | WS_THICKFRAME,0,0,560,720,None,None,None,None).unwrap() };
        let _root = OwnedWindow(root);
        let child = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(),w!("STATIC"),w!("synthetic CEF viewport"),
            WS_CHILD | WS_VISIBLE,0,0,560,720,Some(root),None,None,None).unwrap() };
        let mut before = RECT::default();
        let mut child_before = RECT::default();
        unsafe { GetClientRect(root,&mut before).unwrap(); GetWindowRect(child,&mut child_before).unwrap(); }
        let style = CaptionStyle { background:[255,255,255],symbols:[36,36,36],height:44 };
        unsafe { configure(root.0 as usize,style).unwrap(); }
        let caption = WINDOWS.with(|windows| windows.borrow().get(&(root.0 as usize)).unwrap().clone());
        let mut after = RECT::default();
        let mut child_after = RECT::default();
        unsafe { GetClientRect(root,&mut after).unwrap(); GetWindowRect(child,&mut child_after).unwrap(); }
        assert_eq!(before,after);
        assert_eq!(child_before,child_after);
        let point = POINT { x:child_before.left+20,y:child_before.top+20 };
        let packed = LPARAM(((point.y as u16 as u32)<<16 | point.x as u16 as u32) as isize);
        assert_eq!(unsafe { SendMessageW(child,WM_NCHITTEST,None,Some(packed)) }.0,HTCAPTION as isize);
        let mut title = [0u16;256];
        assert!(unsafe { GetWindowTextW(caption.button.get(),&mut title) } > 0);
        let mut accessible = std::ptr::null_mut();
        unsafe { AccessibleObjectFromWindow(caption.button.get(),OBJID_CLIENT.0 as u32,&IAccessible::IID,&mut accessible).unwrap(); }
        let accessible = unsafe { IAccessible::from_raw(accessible) };
        assert_eq!(unsafe { accessible.accChildCount().unwrap() },0);
        let dark = CaptionStyle { background:[30,30,30],symbols:[228,228,228],height:44 };
        unsafe { update(root.0 as usize,dark).unwrap(); }
        assert_eq!(caption.style.get().background,[30,30,30]);
        unsafe { SendMessageW(root,WM_NCLBUTTONDBLCLK,Some(WPARAM(HTCAPTION as usize)),Some(packed)); }
        assert_eq!(unsafe { GetWindowLongPtrW(root,GWL_STYLE) } as u32 & WS_MAXIMIZE.0,0);
        unsafe { SendMessageW(caption.button.get(),BM_CLICK,None,None); }
        let mut close = MSG::default();
        assert!(unsafe { PeekMessageW(&mut close,Some(root),WM_CLOSE,WM_CLOSE,PM_REMOVE) }.as_bool());
        assert!(unsafe { IsWindow(Some(root)) }.as_bool());
        drop(accessible);
        unsafe { DispatchMessageW(&close); }
        assert!(!unsafe { IsWindow(Some(root)) }.as_bool());
        assert!(!WINDOWS.with(|windows| windows.borrow().contains_key(&(root.0 as usize))));
        assert!(unsafe { update(root.0 as usize,dark) }.is_err());
    }
}
