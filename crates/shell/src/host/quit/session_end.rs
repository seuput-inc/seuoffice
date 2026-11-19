//! The OS ending the session: Windows shutdown, restart and sign-out
//! (`session-end` of the Electron app) and macOS shutdown, restart and log out
//! (the `shutdown` event of Electron's `powerMonitor`, which is
//! `NSWorkspaceWillPowerOffNotification` there).
//!
//! The OS asks no editor anything here: there is no save prompt, every editor
//! writes its recovery copies now, nothing deletes them, and the next start
//! treats the run as ended by the OS. tao answers `WM_ENDSESSION` by ending the
//! process at once, so without a subclass of the window procedure the copies
//! would never be written. On macOS the process ends through AppKit's
//! `terminate:` that follows the notification, which `terminate.rs` lets pass
//! once this work has started.
//!
//! Only `WM_ENDSESSION` with the session really ending starts the work on
//! Windows: a shutdown another program cancels (`WM_QUERYENDSESSION` answered
//! no, or `wParam` false) leaves the running app untouched. The UI thread keeps
//! pumping messages while it waits, because the editors' answers reach the core
//! thread through it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use main_core::packages::electron_utils::recovery_store::SharedRecoveryState;
use main_core::shell::launch_recovery::OS_SESSION_END_WRITE_TIMEOUT_MS;
#[cfg(target_os = "macos")]
use objc2::msg_send;
#[cfg(target_os = "macos")]
use objc2::runtime::{AnyClass, AnyObject, Bool};
#[cfg(target_os = "macos")]
use objc2_foundation::NSString;
use shell_ipc_w::projects::Projects;
use tauri::AppHandle;
#[cfg(windows)]
use tauri::Manager;
#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
#[cfg(windows)]
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WM_ENDSESSION,
};

/// An arbitrary id: one subclass of this module per window
#[cfg(windows)]
const SUBCLASS_ID: usize = 0x5155_4954;

/// The copies were asked for; every window of the process gets the message but
/// the work runs once
static STARTED: AtomicBool = AtomicBool::new(false);
/// How the work ended, once it has
static RESULT: Mutex<Option<Result<(), String>>> = Mutex::new(None);

/// Subclasses the windows that exist now. May be called from any thread: the
/// Win32 work runs on the UI thread, which owns the windows.
#[cfg(windows)]
pub(super) fn install(app: &AppHandle) {
    let target = app.clone();
    let result = app.run_on_main_thread(move || {
        for window in target.windows().into_values() {
            if let Err(error) = watch(&window) {
                eprintln!("[quit] a Windows session end is not guarded: {error}");
            }
        }
    });
    if let Err(error) = result {
        eprintln!("[quit] a Windows session end is not guarded: {error}");
    }
}

#[cfg(windows)]
fn watch(window: &tauri::Window) -> Result<(), String> {
    let raw = window.hwnd().map_err(|error| error.to_string())?;
    // Tauri's `HWND` may come from another `windows` version; only the value crosses over.
    let hwnd = HWND(raw.0);
    // SAFETY: a live window of this thread; `session_proc` has the subclass
    // procedure signature and keeps no per-window data
    if unsafe { SetWindowSubclass(hwnd, Some(session_proc), SUBCLASS_ID, 0) }.as_bool() {
        Ok(())
    } else {
        Err("SetWindowSubclass refused the window".into())
    }
}

#[cfg(windows)]
unsafe extern "system" fn session_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_ENDSESSION && wparam.0 != 0 {
        begin();
        wait();
    }
    // SAFETY: the next procedure of the subclass chain, with the message as received
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// Observes the power-off notification of the workspace for the life of the
/// process. May be called from any thread: the AppKit work runs on the UI thread.
#[cfg(target_os = "macos")]
pub(super) fn install(app: &AppHandle) {
    let result = app.run_on_main_thread(|| {
        if let Err(error) = observe() {
            eprintln!("[quit] a macOS session end is not guarded: {error}");
        }
    });
    if let Err(error) = result {
        eprintln!("[quit] a macOS session end is not guarded: {error}");
    }
}

#[cfg(target_os = "macos")]
fn observe() -> Result<(), String> {
    let class = AnyClass::get(c"NSWorkspace").ok_or("the workspace class is unavailable")?;
    // SAFETY: `sharedWorkspace` and `notificationCenter` take no arguments and
    // return objects; the notification center of the workspace is not the default one
    let workspace: *mut AnyObject = unsafe { msg_send![class, sharedWorkspace] };
    if workspace.is_null() {
        return Err("the shared workspace is unavailable".into());
    }
    let center: *mut AnyObject = unsafe { msg_send![workspace, notificationCenter] };
    if center.is_null() {
        return Err("the workspace notification center is unavailable".into());
    }
    let callback = block2::RcBlock::new(|_: *mut AnyObject| {
        begin();
        wait();
    });
    let name = NSString::from_str("NSWorkspaceWillPowerOffNotification");
    // SAFETY: the selector takes a name, an optional sender and queue, and a
    // block; the center copies the block and keeps the observer for the life
    // of the process, so neither is released here
    let observer: *mut AnyObject = unsafe {
        msg_send![center, addObserverForName: &*name, object: std::ptr::null::<AnyObject>(),
        queue: std::ptr::null::<AnyObject>(), usingBlock: &*callback]
    };
    if observer.is_null() {
        return Err("the power-off observer was not registered".into());
    }
    Ok(())
}

/// The work has started, so the process is being ended by the OS and no
/// unsaved-changes guard may be asked any more
#[cfg(target_os = "macos")]
pub(super) fn started() -> bool {
    STARTED.load(Ordering::Acquire)
}

fn begin() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(services) = super::SERVICES.get() else {
        return finish(Err("quit services are unavailable".into()));
    };
    let Some(shared) = services
        .hosts
        .get::<Arc<Mutex<SharedRecoveryState>>>()
        .cloned()
    else {
        return finish(Err("the recovery state is unavailable".into()));
    };
    crate::host::w::post_core_checked(
        move |provided, services| {
            let begun = crate::host::recovery_launch::begin_os_session_end(
                provided,
                shared,
                services.core.clone(),
                Box::new(|result| copies_written(result.map_err(|failure| failure.0))),
            );
            if let Err(failure) = begun {
                finish(Err(failure.0));
            }
        },
        || finish(Err("the core thread refused the session end".into())),
    );
}

/// On core, after every editor answered or the timeout passed: the run ended on
/// purpose, then the project list order is written as `releaseAppResources` did
fn copies_written(result: Result<(), String>) {
    crate::host::crash::mark_os_ended_on_core();
    let mut pending = Some(result);
    crate::host::w::on_core_local(|provided| {
        if let Some(projects) = provided.all::<Projects>().into_iter().next()
            && let Some(result) = pending.take()
        {
            // best-effort: the list order is only a convenience
            projects.flush_touches(Box::new(move |_| finish(result)));
        }
    });
    if let Some(result) = pending.take() {
        finish(result);
    }
}

fn finish(result: Result<(), String>) {
    if let Err(error) = &result {
        eprintln!("[quit] session end: not every copy was written: {error}");
    }
    *RESULT.lock().unwrap_or_else(|error| error.into_inner()) = Some(result);
}

fn finished() -> bool {
    RESULT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some()
}

/// The editors' own deadline, and a second for the post back to the UI thread
fn deadline() -> Duration {
    Duration::from_millis(OS_SESSION_END_WRITE_TIMEOUT_MS as u64 + 1_000)
}

/// On the UI thread: pumps messages until the copies are written, so the core
/// thread's work that goes through it can finish, but not beyond the editors'
/// own deadline.
#[cfg(windows)]
fn wait() {
    let limit = deadline();
    let started = Instant::now();
    while !finished() && started.elapsed() < limit {
        let mut message = MSG::default();
        // SAFETY: the ordinary message pump of the thread that owns the windows
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !finished() {
        eprintln!("[quit] the session end did not finish within its deadline");
    }
}

/// On the UI thread: runs the main run loop in short turns until the copies are
/// written, so the core thread's work that goes through it can finish, but not
/// beyond the editors' own deadline.
#[cfg(target_os = "macos")]
fn wait() {
    let (Some(run_loop), Some(date)) = (AnyClass::get(c"NSRunLoop"), AnyClass::get(c"NSDate"))
    else {
        return eprintln!("[quit] the session end cannot pump the run loop");
    };
    let mode = NSString::from_str("kCFRunLoopDefaultMode");
    let limit = deadline();
    let started = Instant::now();
    while !finished() && started.elapsed() < limit {
        // SAFETY: the ordinary run loop turn of the main thread, which owns the
        // windows; `runMode:beforeDate:` answers a BOOL
        unsafe {
            let current: *mut AnyObject = msg_send![run_loop, currentRunLoop];
            let until: *mut AnyObject = msg_send![date, dateWithTimeIntervalSinceNow: 0.005_f64];
            let _: Bool = msg_send![current, runMode: &*mode, beforeDate: until];
        }
    }
    if !finished() {
        eprintln!("[quit] the session end did not finish within its deadline");
    }
}
