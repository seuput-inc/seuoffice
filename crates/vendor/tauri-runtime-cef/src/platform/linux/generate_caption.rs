//! Native controls over the existing Generate viewport; CEF geometry is unchanged.
use std::{cell::RefCell, collections::HashMap, ffi::CString};
use gtk::{glib, prelude::*};
use x11_dl::{xfixes, xlib};

#[derive(Clone, Copy, Debug)]
pub struct CaptionStyle { pub background: [u8; 3], pub symbols: [u8; 3], pub height: u32 }

struct InputShapeLibrary(*mut libc::c_void);
impl InputShapeLibrary {
    fn open() -> Result<Self,String> {
        // libXext installs display-close hooks, so this handle outlives XCloseDisplay.
        let library = unsafe { libc::dlopen(c"libXext.so.6".as_ptr(),libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if library.is_null() { return Err("native caption requires libXext.so.6".into()); }
        Ok(Self(library))
    }
    unsafe fn require_version(&self, display: *mut xlib::Display) -> Result<(),String> {
        let symbol = unsafe { libc::dlsym(self.0,c"XShapeQueryVersion".as_ptr()) };
        if symbol.is_null() { return Err("native caption requires XShapeQueryVersion".into()); }
        // Exact Status(Display*, int*, int*) declaration in libXext-1.3.6 shape.h.
        let query: unsafe extern "C" fn(*mut xlib::Display,*mut libc::c_int,*mut libc::c_int)->libc::c_int =
            unsafe { std::mem::transmute(symbol) };
        let (mut major,mut minor) = (0,0);
        if unsafe { query(display,&mut major,&mut minor) } == 0 || (major,minor) < (1,1) {
            return Err("native caption requires X Shape 1.1 input regions".into());
        }
        Ok(())
    }
}
impl Drop for InputShapeLibrary {
    fn drop(&mut self) { unsafe { libc::dlclose(self.0); } }
}

struct Caption {
    overlay: gtk::Overlay,
    handle: gtk::Box,
    controls: gtk::WindowControls,
    provider: gtk::CssProvider,
    surface: gtk::gdk::Surface,
    signal: Option<glib::SignalHandlerId>,
    style: CaptionStyle,
    xlib: xlib::Xlib,
    fixes: xfixes::Xlib,
    display: *mut xlib::Display,
    _shape: InputShapeLibrary,
}
impl Drop for Caption {
    fn drop(&mut self) {
        if let Some(signal) = self.signal.take() { self.surface.disconnect(signal); }
        // Close while libXext and the Xlib/XFixes symbol owners are still live.
        unsafe { (self.xlib.XCloseDisplay)(self.display); }
    }
}
struct Registration {
    window: glib::WeakRef<gtk::ApplicationWindow>,
    content: gtk::Box,
    caption: Option<Caption>,
}
thread_local! { static WINDOWS: RefCell<HashMap<u64, Registration>> = RefCell::new(HashMap::new()); }

pub(crate) fn register(window: &gtk::ApplicationWindow, content: &gtk::Box, xid: u64) {
    WINDOWS.with(|windows| { windows.borrow_mut().insert(xid, Registration {
        window: window.downgrade(), content: content.clone(), caption: None,
    }); });
}

fn css(xid: u64, style: CaptionStyle) -> String {
    let [r,g,b] = style.background;
    let [sr,sg,sb] = style.symbols;
    format!(".seu-generate-controls-{xid} {{ background: rgb({r},{g},{b}); color: rgb({sr},{sg},{sb}); margin: 0; padding: 0; }} .seu-generate-controls-{xid} button {{ min-width: 46px; min-height: {}px; padding: 0; margin: 0; border-radius: 0; border-width: 0; color: rgb({sr},{sg},{sb}); background: rgb({r},{g},{b}); }} .seu-generate-controls-{xid} button:hover {{ background: #e81123; color: #ffffff; }}", style.height)
}

/// Must run on the GTK UI thread with the live GtkApplicationWindow pointer.
/// The pointer is borrowed only during this call.
pub unsafe fn configure(native: usize, style: CaptionStyle) -> Result<(), String> {
    if style.height != 44 { return Err("Generate caption height must be 44 DIP".into()); }
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let (&xid, registration) = windows.iter_mut().find(|(_, entry)| entry.window.upgrade()
            .is_some_and(|window| window.as_ptr() as usize == native)).ok_or("caption requires the registered X11 CEF window")?;
        if let Some(caption) = registration.caption.as_mut() {
            caption.style = style;
            caption.handle.set_size_request(-1, style.height as i32);
            caption.provider.load_from_data(&css(xid, style));
            return apply_shape(xid, registration);
        }
        let window = registration.window.upgrade().ok_or("caption window was destroyed")?;
        let surface = window.surface().ok_or("caption requires a realized native window")?;
        let xlib = xlib::Xlib::open().map_err(|error| error.to_string())?;
        let fixes = xfixes::Xlib::open().map_err(|error| error.to_string())?;
        let shape = InputShapeLibrary::open()?;
        let display_name = CString::new(window.display().name().as_str()).map_err(|error| error.to_string())?;
        // SAFETY: a separate connection to this GTK window's X server is used only on UI.
        let display = unsafe { (xlib.XOpenDisplay)(display_name.as_ptr()) };
        if display.is_null() { return Err("cannot connect to caption X server".into()); }
        let mut event = 0;
        let mut error = 0;
        // SAFETY: display is live and output pointers are valid.
        if unsafe { (fixes.XFixesQueryExtension)(display, &mut event, &mut error) } == 0 {
            unsafe { (xlib.XCloseDisplay)(display); }
            return Err("native caption requires the XFixes extension".into());
        }
        let mut major = 5;
        let mut minor = 0;
        let available = unsafe { (fixes.XFixesQueryVersion)(display, &mut major, &mut minor) };
        if available == 0 || major < 2 {
            unsafe { (xlib.XCloseDisplay)(display); }
            return Err("native caption requires XFixes region support".into());
        }
        if let Err(error) = unsafe { shape.require_version(display) } {
            unsafe { (xlib.XCloseDisplay)(display); }
            return Err(error);
        }
        let overlay = gtk::Overlay::new();
        overlay.set_hexpand(true); overlay.set_vexpand(true);
        let handle = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        handle.set_halign(gtk::Align::Fill); handle.set_valign(gtk::Align::Start);
        handle.set_size_request(-1, style.height as i32);
        let space = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        space.set_hexpand(true);
        let drag = gtk::GestureClick::new();
        drag.set_button(0);
        drag.connect_pressed(|gesture, count, _, _| {
            let Some(event) = gesture.current_event() else { return; };
            let Some(surface) = event.surface() else { return; };
            let Ok(toplevel) = surface.dynamic_cast::<gtk::gdk::Toplevel>() else { return; };
            match gesture.current_button() {
                1 => {
                    // No maximize action on double click. Generate is resizable,
                    // but the original window explicitly disables maximization.
                    if count == 1 {
                        if let (Some(device), Some((x,y))) = (event.device(), event.position()) {
                            toplevel.begin_move(&device, 1, x, y, event.time());
                        }
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
                3 => {
                    toplevel.show_window_menu(&event);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
                _ => {}
            }
        });
        space.add_controller(drag);
        handle.append(&space);
        // GTK supplies the platform-localized accessible name, action and role.
        let controls = gtk::WindowControls::new(gtk::PackType::End);
        controls.set_decoration_layout(Some(":close"));
        controls.add_css_class(&format!("seu-generate-controls-{xid}"));
        handle.append(&controls);
        window.set_child(gtk::Widget::NONE);
        overlay.set_child(Some(&registration.content));
        overlay.add_overlay(&handle); overlay.set_measure_overlay(&handle, false);
        window.set_child(Some(&overlay));
        let provider = gtk::CssProvider::new();
        provider.load_from_data(&css(xid, style));
        gtk::style_context_add_provider_for_display(&window.display(), &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let signal = surface.connect_layout(move |_, _, _| { layout(xid); });
        registration.caption = Some(Caption { overlay, handle, controls, provider, surface, signal: Some(signal), style, xlib, fixes, display, _shape: shape });
        if let Err(error) = apply_shape(xid, registration) {
            remove_caption(registration);
            return Err(error);
        }
        Ok(())
    })
}

/// Same native window and UI-thread requirements as configure.
pub unsafe fn update(native: usize, style: CaptionStyle) -> Result<(), String> {
    let installed = WINDOWS.with(|windows| windows.borrow().values().any(|entry|
        entry.caption.is_some() && entry.window.upgrade().is_some_and(|window| window.as_ptr() as usize == native)));
    if !installed { return Err("native caption is not configured".into()); }
    unsafe { configure(native, style) }
}

fn rectangles(width: i32, height: i32, header: i32, controls: i32) -> (Vec<xlib::XRectangle>, Vec<xlib::XRectangle>) {
    let h = header.clamp(0, height);
    let controls = controls.clamp(0, width);
    let body = xlib::XRectangle { x: 0, y: h as i16, width: width as u16, height: (height-h) as u16 };
    let title = xlib::XRectangle { x: 0, y: 0, width: (width-controls) as u16, height: h as u16 };
    (vec![title, body], vec![body])
}

fn apply_shape(xid: u64, registration: &Registration) -> Result<(), String> {
    let Some(caption) = registration.caption.as_ref() else { return Ok(()); };
    let Some(window) = registration.window.upgrade() else { return Err("caption window was destroyed".into()); };
    let scale = caption.surface.scale_factor().max(1);
    let logical_width = registration.content.width();
    let logical_height = registration.content.height();
    let width = if logical_width > 0 { logical_width } else { window.default_width().max(1) };
    let height = if logical_height > 0 { logical_height } else { window.default_height().max(1) };
    let width = width.checked_mul(scale).ok_or("caption width overflow")?;
    let height = height.checked_mul(scale).ok_or("caption height overflow")?;
    if width > u16::MAX as i32 || height > u16::MAX as i32 { return Err("caption shape exceeds X11 extent range".into()); }
    let header = (caption.style.height as i32).checked_mul(scale).ok_or("caption height overflow")?.min(height);
    if header > i16::MAX as i32 { return Err("caption band exceeds X11 coordinate range".into()); }
    let controls = caption.controls.width().max(46).checked_mul(scale).ok_or("caption control width overflow")?;
    let (mut bounding, mut input) = rectangles(width, height, header, controls);
    // ShapeBounding=0 and ShapeInput=2 are the X Shape protocol constants.
    // Only the controls rectangle is visually clipped. The CEF viewport and its
    // children are neither moved nor resized; the original HTML title stays drawn.
    unsafe {
        let visible = (caption.fixes.XFixesCreateRegion)(caption.display, bounding.as_mut_ptr(), bounding.len() as i32);
        let mouse = (caption.fixes.XFixesCreateRegion)(caption.display, input.as_mut_ptr(), input.len() as i32);
        (caption.fixes.XFixesSetWindowShapeRegion)(caption.display, xid as _, 0, 0, 0, visible);
        (caption.fixes.XFixesSetWindowShapeRegion)(caption.display, xid as _, 2, 0, 0, mouse);
        (caption.fixes.XFixesDestroyRegion)(caption.display, visible);
        (caption.fixes.XFixesDestroyRegion)(caption.display, mouse);
        (caption.xlib.XFlush)(caption.display);
    }
    Ok(())
}

pub(crate) fn layout(xid: u64) {
    WINDOWS.with(|windows| {
        let Ok(windows) = windows.try_borrow() else { return; };
        if let Some(registration) = windows.get(&xid) {
            if let Err(error) = apply_shape(xid, registration) { eprintln!("native caption layout: {error}"); }
        }
    });
}

fn remove_caption(registration: &mut Registration) {
    if let Some(mut caption) = registration.caption.take() {
        if let Some(signal) = caption.signal.take() { caption.surface.disconnect(signal); }
        gtk::style_context_remove_provider_for_display(&caption.surface.display(), &caption.provider);
        if let Some(window) = registration.window.upgrade() {
            window.set_child(gtk::Widget::NONE);
            caption.overlay.set_child(gtk::Widget::NONE);
            window.set_child(Some(&registration.content));
        }
    }
}

pub(crate) fn unregister(xid: u64) {
    let registration = WINDOWS.with(|windows| windows.borrow_mut().remove(&xid));
    if let Some(mut registration) = registration { remove_caption(&mut registration); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_caption_controls_are_visually_clipped() {
        let (visible, input) = rectangles(560, 720, 44, 46);
        assert_eq!((visible[0].width, visible[0].height), (514,44));
        assert_eq!((visible[1].y, visible[1].height), (44,676));
        assert_eq!((input[0].y,input[0].height), (44,676));
    }
    #[test]
    fn shape_scales_but_does_not_remove_the_title_or_move_document_pixels() {
        let (visible,input) = rectangles(1120,1440,88,92);
        assert_eq!((visible[0].x,visible[0].y,visible[0].width,visible[0].height),(0,0,1028,88));
        assert_eq!((visible[1].x,visible[1].y,visible[1].width,visible[1].height),(0,88,1120,1352));
        assert_eq!((input[0].x,input[0].y,input[0].width,input[0].height),(0,88,1120,1352));
    }
    #[test]
    fn palette_rules_do_not_change_another_native_window() {
        let style = CaptionStyle { background:[30,30,30],symbols:[228,228,228],height:44 };
        let rules = css(17,style);
        assert!(rules.contains(".seu-generate-controls-17 button"));
        assert!(!rules.contains(".seu-generate-controls-18"));
        assert!(rules.contains("rgb(30,30,30)"));
    }
}
