use std::{cell::RefCell, ffi::c_void, path::PathBuf, sync::Arc};
use block2::RcBlock;
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{NSAlert, NSApplication, NSApplicationActivationPolicy, NSOpenPanel, NSSavePanel, NSWindow};
use objc2_foundation::{NSArray, NSString, NSURL};
use super::{FileSpec, NativeSessions, Session};
use tauri_runtime_cef::WindowCefExt;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetMain() -> *const c_void;
    static kCFRunLoopCommonModes: *const c_void;
    fn CFRunLoopPerformBlock(run_loop: *const c_void, mode: *const c_void, block: *const c_void);
    fn CFRunLoopWakeUp(run_loop: *const c_void);
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" { fn CGShieldingWindowLevel() -> i32; }

#[derive(Clone)]
enum Target { Panel(Retained<NSSavePanel>), Alert(Retained<NSAlert>) }
thread_local! { static ACTIVE: RefCell<NativeSessions<Target>> = RefCell::new(NativeSessions::new()); }

pub(crate) struct Registration(u64);
impl Drop for Registration {
    fn drop(&mut self) {
        let removed = ACTIVE.with(|active| active.borrow_mut().remove(&self.0));
        drop(removed);
    }
}

pub(super) fn prepare(session: &Arc<Session>) -> Result<(), String> {
    MainThreadMarker::new().ok_or("Native dialog is not on the main thread.")?;
    session.check()
}

pub(super) fn wake(session: &Session) {
    let id = session.id;
    // These blocks contain only an immutable non-reused ID. Late delivery cannot
    // retain or close a newer dialog, even when AppKit reuses a panel instance.
    let block = RcBlock::new(move || dismiss(id));
    unsafe {
        let run_loop = CFRunLoopGetMain();
        CFRunLoopPerformBlock(run_loop, kCFRunLoopCommonModes, RcBlock::as_ptr(&block).cast());
        let modal = NSString::from_str("NSModalPanelRunLoopMode");
        CFRunLoopPerformBlock(run_loop, (&*modal as *const NSString).cast(), RcBlock::as_ptr(&block).cast());
        CFRunLoopWakeUp(run_loop);
    }
}

fn dismiss(id: u64) {
    let Some(mtm) = MainThreadMarker::new() else { return; };
    let target = ACTIVE.with(|active| active.borrow().get(&id).cloned());
    match target {
        Some(Target::Panel(panel)) => unsafe { panel.cancel(None); },
        Some(Target::Alert(alert)) => {
            let window = alert.window();
            let app = NSApplication::sharedApplication(mtm);
            if let Some(parent) = window.sheetParent() { parent.endSheet_returnCode(&window, 0); }
            if app.modalWindow().as_deref().is_some_and(|modal| std::ptr::eq(modal, &*window)) {
                app.abortModal();
            }
            window.orderOut(None);
        }
        None => {}
    }
}

pub(crate) fn register_alert(session: &Arc<Session>, alert: &NSAlert) -> Result<Registration, String> {
    session.check()?;
    let alert = unsafe { Retained::retain(alert as *const NSAlert as *mut NSAlert) }.ok_or("Native alert is unavailable.")?;
    ACTIVE.with(|active| active.borrow_mut().insert(session.id, Target::Alert(alert)))?;
    Ok(Registration(session.id))
}

pub(crate) fn alert_is_registered(id: u64, window: &NSWindow) -> bool {
    let target = ACTIVE.with(|active| active.borrow().get(&id).cloned());
    match target {
        Some(Target::Alert(alert)) => std::ptr::eq(&*alert.window(), window),
        _ => false,
    }
}

pub(super) fn files(app: &tauri::AppHandle, spec: &FileSpec, parent: Option<&tauri::Window>, save: bool, session: &Arc<Session>) -> Result<Option<Vec<PathBuf>>, String> {
    let previous = tauri_runtime_cef::native_app_key_window_handle(app).map_err(|error| error.to_string())?;
    let result = match parent {
        Some(parent) => parent.native_window_handle().map_err(|error| error.to_string())?
            .with_ns_window(|window| files_with_parent(spec, Some(window), save, session))
            .map_err(|error| error.to_string())?,
        None => files_with_parent(spec, None, save, session),
    }?;
    if !session.cancelled() && let Some(window) = previous {
        window.with_ns_window(|native| native.makeKeyAndOrderFront(None)).map_err(|error| error.to_string())?;
    }
    Ok(result)
}

fn files_with_parent(spec: &FileSpec, parent: Option<&NSWindow>, save: bool, session: &Arc<Session>) -> Result<Option<Vec<PathBuf>>, String> {
    let mtm = MainThreadMarker::new().ok_or("Native dialog is not on the main thread.")?;
    let app = NSApplication::sharedApplication(mtm);
    let previous_policy = unsafe { app.activationPolicy() };
    let panel: Retained<NSSavePanel> = if save { NSSavePanel::savePanel(mtm) }
        else { Retained::into_super(NSOpenPanel::openPanel(mtm)) };
    if !save {
        let open = panel.downcast_ref::<NSOpenPanel>().ok_or("Native open panel is unavailable.")?;
        open.setCanChooseDirectories(spec.folders);
        open.setCanChooseFiles(!spec.folders);
        open.setAllowsMultipleSelection(spec.multiple);
    }
    if save || !spec.folders {
        let extensions: Vec<_> = spec.filters.iter().flat_map(|(_, extensions)| extensions.iter()).map(|extension| NSString::from_str(extension)).collect();
        if !extensions.is_empty() {
            #[allow(deprecated)]
            panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&extensions)));
        }
    }
    if let Some(title) = &spec.title { panel.setMessage(Some(&NSString::from_str(title))); }
    if let Some(create) = spec.create_directories.or((!save && spec.folders).then_some(true)) { panel.setCanCreateDirectories(create); }
    if let Some(directory) = &spec.directory {
        let path = spec.name.as_ref().map_or_else(|| directory.clone(), |name| directory.join(name));
        if let Some(path) = path.to_str() {
            let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), true);
            panel.setDirectoryURL(Some(&url));
        }
    }
    if (save || (!spec.folders && !spec.multiple)) && let Some(name) = &spec.name {
        panel.setNameFieldStringValue(&NSString::from_str(name));
    }
    session.check()?;
    ACTIVE.with(|active| active.borrow_mut().insert(session.id, Target::Panel(panel.clone())))?;
    let registration = Registration(session.id);
    if previous_policy == NSApplicationActivationPolicy::Prohibited { app.setActivationPolicy(NSApplicationActivationPolicy::Accessory); }
    panel.setLevel(unsafe { CGShieldingWindowLevel() } as isize);
    // Match rfd's sheet and modal session while retaining the exact cancel target.
    let completion = RcBlock::new(|_: isize| {});
    if let Some(parent) = parent { panel.beginSheetModalForWindow_completionHandler(parent, &completion); }
    let response = if session.cancelled() { 0 } else { panel.runModal() };
    if let Some(parent) = panel.sheetParent() { parent.endSheet_returnCode(&panel, response); }
    panel.orderOut(None);
    drop(registration);
    app.setActivationPolicy(previous_policy);
    let result = if response != 1 { None }
        else if save { Some(vec![url_path(panel.URL().ok_or("Native save panel returned no URL.")?)?]) }
        else {
            let open = panel.downcast_ref::<NSOpenPanel>().ok_or("Native open panel is unavailable.")?;
            Some(open.URLs().iter().map(|url| url_path_ref(&url)).collect::<Result<Vec<_>, _>>()?)
        };
    Ok(result)
}

fn url_path(url: Retained<NSURL>) -> Result<PathBuf, String> { url_path_ref(&url) }
fn url_path_ref(url: &NSURL) -> Result<PathBuf, String> {
    url.path().map(|path| PathBuf::from(path.to_string())).ok_or_else(|| "Native dialog URL has no filesystem path.".into())
}
