//! The macOS quits that do not come through the menu: the Dock's Quit and log
//! out or shut down. AppKit sends the app `terminate:`, and tao's delegate has
//! no `applicationShouldTerminate:`, so the process would end at
//! `applicationWillTerminate` without a single unsaved-changes guard and without
//! the `ExitRequested` event the quit cleanup waits on.
//!
//! This adds that method to the class of whatever object is the NSApp delegate
//! (tao's, set by Tauri) with the Objective-C runtime, as the Dock menu does. It
//! cancels AppKit's terminate and runs the guarded quit instead; the process
//! ends through tao's own exit, which does not call `terminate:` again.

use std::mem::transmute;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{ffi, sel};
use objc2_app_kit::NSApplication;
use tauri::AppHandle;

/// `applicationShouldTerminate:`: `(self, _cmd, sender) -> NSApplicationTerminateReply`
type ShouldTerminateImp = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize;

/// `NSTerminateCancel`
const TERMINATE_CANCEL: usize = 0;

static APP: OnceLock<AppHandle> = OnceLock::new();

/// Adds the method to the delegate's class. May be called from any thread: the
/// AppKit work runs on the UI thread.
pub(super) fn install(app: &AppHandle) {
    let _ = APP.set(app.clone());
    let result = app.run_on_main_thread(|| {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return;
        };
        let Some(delegate) = NSApplication::sharedApplication(mtm).delegate() else {
            eprintln!("[quit] the app has no delegate, so a Dock or log out quit is not guarded");
            return;
        };
        // SAFETY: a protocol object is an Objective-C object
        let object: &AnyObject = unsafe { &*(Retained::as_ptr(&delegate).cast::<AnyObject>()) };
        add(object.class());
    });
    if let Err(error) = result {
        eprintln!("[quit] a Dock or log out quit is not guarded: {error}");
    }
}

fn add(class: &AnyClass) {
    let selector = sel!(applicationShouldTerminate:);
    if class.instance_method(selector).is_some() {
        // already answered by the delegate (or added before): not ours to replace
        return;
    }
    // SAFETY: `should_terminate` has the signature of `applicationShouldTerminate:`
    // ("Q@:@"); a method added to a class is only ever called by the runtime
    let added = unsafe {
        ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            selector,
            transmute::<ShouldTerminateImp, Imp>(should_terminate),
            c"Q@:@".as_ptr(),
        )
    };
    if !added.as_bool() {
        eprintln!("[quit] the delegate refused the terminate hook; a Dock or log out quit is not guarded");
    }
}

unsafe extern "C-unwind" fn should_terminate(
    _this: *mut AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) -> usize {
    if let Some(app) = APP.get() {
        super::request_quit(app);
    }
    TERMINATE_CANCEL
}

#[cfg(test)]
mod tests {
    use objc2::ClassType;
    use objc2::msg_send;
    use objc2::runtime::ClassBuilder;
    use objc2_foundation::NSObject;

    use super::*;

    #[test]
    fn a_delegate_class_cancels_the_appkit_terminate_once_the_hook_is_added() {
        let class = ClassBuilder::new(c"QuitTestDelegate", NSObject::class())
            .expect("unique test class name")
            .register();
        add(class);
        // a second add is a no-op
        add(class);
        let object: Retained<AnyObject> = unsafe { msg_send![class, new] };
        let reply: usize = unsafe {
            msg_send![&*object, applicationShouldTerminate: std::ptr::null_mut::<AnyObject>()]
        };
        assert_eq!(reply, TERMINATE_CANCEL);
    }
}
