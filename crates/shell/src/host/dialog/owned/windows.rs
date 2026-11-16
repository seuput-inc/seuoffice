use super::{FileSpec, NativeSessions, Session};
use crate::native::WindowExt;
use std::{
    cell::RefCell,
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    sync::{Arc, OnceLock, atomic::Ordering},
};
use windows::{
    Win32::{
        Foundation::{ERROR_CANCELLED, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
            LibraryLoader::GetModuleHandleW,
        },
        UI::{
            Controls::{TASKDIALOG_NOTIFICATIONS, TDM_CLICK_BUTTON, TDN_CREATED, TDN_DESTROYED},
            Shell::{
                Common::COMDLG_FILTERSPEC, FOS_ALLOWMULTISELECT, FOS_PICKFOLDERS, FileOpenDialog,
                FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
                SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, HWND_MESSAGE, PostMessageW, RegisterClassW,
                SendMessageW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WNDCLASSW,
            },
        },
    },
    core::{HRESULT, Interface, PCWSTR, w},
};

const CANCEL: u32 = WM_APP + 0x6d;
static CLASS: OnceLock<Result<(), String>> = OnceLock::new();
#[derive(Clone)]
enum Target {
    File(IFileDialog),
    Task(HWND),
}
thread_local! {
    static ACTIVE: RefCell<NativeSessions<Target>> = RefCell::new(NativeSessions::new());
    static DISPATCHER: RefCell<Option<HWND>> = const { RefCell::new(None) };
}

pub(crate) struct Registration(u64);
impl Drop for Registration {
    fn drop(&mut self) {
        let removed = ACTIVE.with(|active| active.borrow_mut().remove(&self.0));
        drop(removed);
    }
}

fn cancelled_hresult() -> HRESULT {
    HRESULT::from_win32(ERROR_CANCELLED.0)
}

unsafe extern "system" fn dispatch(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == CANCEL {
        dismiss(wparam.0 as u64);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

pub(super) fn prepare(session: &Arc<Session>) -> Result<(), String> {
    session.check()?;
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
    CLASS
        .get_or_init(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(dispatch),
                hInstance: HINSTANCE(instance.0),
                lpszClassName: w!("SeuOfficeOwnedDialogCancellation"),
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0 {
                Err(windows::core::Error::from_thread().to_string())
            } else {
                Ok(())
            }
        })
        .clone()?;
    let hwnd = DISPATCHER.with(|dispatcher| -> Result<HWND, String> {
        if let Some(hwnd) = *dispatcher.borrow() {
            return Ok(hwnd);
        }
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("SeuOfficeOwnedDialogCancellation"),
                PCWSTR::null(),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(HINSTANCE(instance.0)),
                None,
            )
        }
        .map_err(|error| error.to_string())?;
        *dispatcher.borrow_mut() = Some(hwnd);
        Ok(hwnd)
    })?;
    // One message-only HWND per UI thread lives until process exit. It is never
    // recycled while a cancellation producer may still hold its address.
    session.dispatcher.store(hwnd.0 as isize, Ordering::Release);
    session.check()
}

pub(super) fn wake(session: &Session) {
    let route = session.dispatcher.load(Ordering::Acquire);
    if route == 0 {
        return;
    }
    if let Err(error) = unsafe {
        PostMessageW(
            Some(HWND(route as *mut _)),
            CANCEL,
            WPARAM(session.id as usize),
            LPARAM(0),
        )
    } {
        eprintln!("shell: scheduling native dialog cancellation: {error}");
    }
}

fn dismiss(id: u64) {
    let target = ACTIVE.with(|active| active.borrow().get(&id).cloned());
    match target {
        Some(Target::File(dialog)) => {
            if let Err(error) = unsafe { dialog.Close(cancelled_hresult()) } {
                // Do not acknowledge cancellation while the native Show call is alive.
                eprintln!("shell: closing owned file dialog: {error}");
            }
        }
        Some(Target::Task(hwnd)) => {
            unsafe {
                SendMessageW(
                    hwnd,
                    TDM_CLICK_BUTTON.0 as u32,
                    Some(WPARAM(2)),
                    Some(LPARAM(0)),
                )
            };
        }
        None => {}
    }
}

pub(crate) unsafe extern "system" fn task_callback(
    hwnd: HWND,
    message: TASKDIALOG_NOTIFICATIONS,
    _: WPARAM,
    _: LPARAM,
    data: isize,
) -> HRESULT {
    // TASKDIALOGCONFIG borrows this Arc for the entire TaskDialogIndirect call.
    let session = unsafe { &*(data as *const Arc<Session>) };
    if message == TDN_CREATED {
        if let Err(error) =
            ACTIVE.with(|active| active.borrow_mut().insert(session.id, Target::Task(hwnd)))
        {
            eprintln!("shell: registering owned TaskDialog: {error}");
        }
        if session.cancelled() {
            wake(session);
        }
    } else if message == TDN_DESTROYED {
        let removed = ACTIVE.with(|active| active.borrow_mut().remove(&session.id));
        drop(removed);
    }
    HRESULT(0)
}

pub(crate) fn task_registration(session: &Arc<Session>) -> Result<Registration, String> {
    session.check()?;
    Ok(Registration(session.id))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Run `job` with the HWND of `window` as one admitted effect on the exact window.
pub(crate) fn with_hwnd<R>(
    window: &tauri::Window,
    job: impl FnOnce(HWND) -> R,
) -> Result<R, String> {
    use raw_window_handle::RawWindowHandle;
    window
        .native_window_handle()?
        .with_window(|handle| {
            match handle
                .window_handle()
                .map_err(|error| error.to_string())?
                .as_raw()
            {
                RawWindowHandle::Win32(win32) => {
                    Ok(job(HWND(win32.hwnd.get() as *mut std::ffi::c_void)))
                }
                _ => Err("The window has no Win32 handle.".to_owned()),
            }
        })
        .map_err(|error| error.to_string())?
}

pub(super) fn files(
    _app: &tauri::AppHandle,
    spec: &FileSpec,
    parent: Option<&tauri::Window>,
    save: bool,
    session: &Arc<Session>,
) -> Result<Option<Vec<PathBuf>>, String> {
    match parent {
        Some(parent) => with_hwnd(parent, |window| {
            files_with_parent(spec, Some(window), save, session)
        })?,
        None => files_with_parent(spec, None, save, session),
    }
}

fn files_with_parent(
    spec: &FileSpec,
    parent: Option<HWND>,
    save: bool,
    session: &Arc<Session>,
) -> Result<Option<Vec<PathBuf>>, String> {
    let open: Option<IFileOpenDialog> = if save {
        None
    } else {
        Some(
            unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| error.to_string())?,
        )
    };
    let dialog: IFileDialog = if let Some(open) = &open {
        open.cast().map_err(|error| error.to_string())?
    } else {
        let save: IFileSaveDialog =
            unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| error.to_string())?;
        save.cast().map_err(|error| error.to_string())?
    };
    unsafe {
        if !save && spec.folders {
            let mut options = FOS_PICKFOLDERS;
            if spec.multiple {
                options |= FOS_ALLOWMULTISELECT;
            }
            dialog
                .SetOptions(options)
                .map_err(|error| error.to_string())?;
        } else {
            if !save && spec.multiple {
                dialog
                    .SetOptions(FOS_ALLOWMULTISELECT)
                    .map_err(|error| error.to_string())?;
            }
            let mut filters: Vec<_> = spec
                .filters
                .iter()
                .map(|(name, extensions)| {
                    (
                        wide(name),
                        wide(
                            &extensions
                                .iter()
                                .map(|extension| format!("*.{extension}"))
                                .collect::<Vec<_>>()
                                .join(";"),
                        ),
                    )
                })
                .collect();
            if let Some(extension) = spec
                .filters
                .first()
                .and_then(|(_, extensions)| extensions.first())
            {
                dialog
                    .SetDefaultExtension(PCWSTR(wide(extension).as_ptr()))
                    .map_err(|error| error.to_string())?;
            }
            if filters.is_empty() {
                filters.push((wide("All Files"), wide("*.*")));
            }
            let native: Vec<_> = filters
                .iter()
                .map(|(name, pattern)| COMDLG_FILTERSPEC {
                    pszName: PCWSTR(name.as_ptr()),
                    pszSpec: PCWSTR(pattern.as_ptr()),
                })
                .collect();
            dialog
                .SetFileTypes(&native)
                .map_err(|error| error.to_string())?;
            if let Some(name) = &spec.name {
                dialog
                    .SetFileName(PCWSTR(wide(name).as_ptr()))
                    .map_err(|error| error.to_string())?;
            }
        }
        if let Some(directory) = spec
            .directory
            .as_deref()
            .and_then(|directory| directory.to_str())
        {
            let directory = directory.strip_prefix(r"\\?\").unwrap_or(directory);
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                PCWSTR(wide(directory).as_ptr()),
                None,
            ) {
                dialog.SetFolder(&item).map_err(|error| error.to_string())?;
            }
        }
        if let Some(title) = &spec.title {
            dialog
                .SetTitle(PCWSTR(wide(title).as_ptr()))
                .map_err(|error| error.to_string())?;
        }
    }
    session.check()?;
    ACTIVE.with(|active| {
        active
            .borrow_mut()
            .insert(session.id, Target::File(dialog.clone()))
    })?;
    let registration = Registration(session.id);
    let shown = if session.cancelled() {
        Err(windows::core::Error::from(cancelled_hresult()))
    } else {
        unsafe { dialog.Show(parent) }
    };
    drop(registration);
    if let Err(error) = shown {
        if error.code() == cancelled_hresult() {
            return Ok(None);
        }
        return Err(error.to_string());
    }
    let paths = unsafe {
        if let Some(open) = open.filter(|_| spec.multiple) {
            let items = open.GetResults().map_err(|error| error.to_string())?;
            let count = items.GetCount().map_err(|error| error.to_string())?;
            let mut paths = Vec::with_capacity(count as usize);
            for index in 0..count {
                paths.push(item_path(
                    &items.GetItemAt(index).map_err(|error| error.to_string())?,
                )?);
            }
            paths
        } else {
            vec![item_path(
                &dialog.GetResult().map_err(|error| error.to_string())?,
            )?]
        }
    };
    Ok(Some(paths))
}

unsafe fn item_path(item: &IShellItem) -> Result<PathBuf, String> {
    let name =
        unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(|error| error.to_string())?;
    let mut length = 0;
    unsafe {
        while *name.0.add(length) != 0 {
            length += 1;
        }
    }
    let path = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(name.0, length)
    }));
    unsafe {
        CoTaskMemFree(Some(name.0.cast()));
    }
    Ok(path)
}
