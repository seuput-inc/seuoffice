//! Windows shutdown, restart and sign-out (`session-end` of the Electron app).
//!
//! The OS asks no editor anything here: there is no save prompt, every editor
//! writes its recovery copies now, nothing deletes them, and the next start
//! treats the run as ended by the OS. tao answers `WM_ENDSESSION` by ending the
//! process at once, so without this subclass the copies would never be written.
//!
//! Only `WM_ENDSESSION` with the session really ending starts the work: a
//! shutdown another program cancels (`WM_QUERYENDSESSION` answered no, or
//! `wParam` false) leaves the running app untouched. The UI thread keeps pumping
//! messages while it waits, because the editors' answers reach the core thread
//! through it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use main_core::packages::electron_utils::recovery_store::SharedRecoveryState;
use main_core::shell::launch_recovery::OS_SESSION_END_WRITE_TIMEOUT_MS;
use shell_ipc_w::projects::Projects;
use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WM_ENDSESSION,
};

/// An arbitrary id: one subclass of this module per window
const SUBCLASS_ID: usize = 0x5155_4954;

/// The copies were asked for; every window of the process gets the message but
/// the work runs once
static STARTED: AtomicBool = AtomicBool::new(false);
/// How the work ended, once it has
static RESULT: Mutex<Option<Result<(), String>>> = Mutex::new(None);

/// Subclasses the windows that exist now. May be called from any thread: the
/// Win32 work runs on the UI thread, which owns the windows.
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

fn watch(window: &tauri::Window) -> Result<(), String> {
    let raw = window.hwnd().map_err(|error| error.to_string())?;
    // Tauri's `HWND` may come from another `windows` version; only the value crosses over.
    let hwnd = HWND(raw.0 as *mut c_void);
    // SAFETY: a live window of this thread; `session_proc` has the subclass
    // procedure signature and keeps no per-window data
    if unsafe { SetWindowSubclass(hwnd, Some(session_proc), SUBCLASS_ID, 0) }.as_bool() {
        Ok(())
    } else {
        Err("SetWindowSubclass refused the window".into())
    }
}

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

/// On the UI thread: pumps messages until the copies are written, so the core
/// thread's work that goes through it can finish, but not beyond the editors'
/// own deadline.
fn wait() {
    let limit = Duration::from_millis(OS_SESSION_END_WRITE_TIMEOUT_MS as u64 + 1_000);
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
