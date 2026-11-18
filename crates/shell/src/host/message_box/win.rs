//! The message box on Windows: `TaskDialogIndirect`, as Electron's
//! `message_box_win.cc` shows it. The buttons are custom ones with ids
//! 1000 + index (the ids of the common buttons are below that), `defaultId`
//! is `nDefaultButton`, the checkbox is the verification text, the icon is
//! the main icon, and `TDF_ALLOW_DIALOG_CANCELLATION` lets Escape and the
//! close box end the dialog with `IDCANCEL`, which is the cancel button.
//! The dialog is modal to the parent window.
//!
//! `TaskDialogIndirect` is in comctl32 version 6, which the executable's
//! manifest asks for (tauri-build's default manifest does).

use std::ffi::c_void;

use shell_contract::hosts::MessageBoxAnswer;
use tauri::Window;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TD_ERROR_ICON, TD_INFORMATION_ICON,
    TD_WARNING_ICON, TDF_ALLOW_DIALOG_CANCELLATION, TDF_SIZE_TO_CONTENT, TaskDialogIndirect,
};
use windows::core::{BOOL, PCWSTR};

use super::{Kind, Spec};
use std::sync::Arc;
use crate::host::dialog::owned::{Session, windows::{task_callback, task_registration}};
use tauri_runtime_cef::WindowCefExt;

/// the id of button `i`
const FIRST_BUTTON: i32 = 1000;
/// what Escape and the close box answer when no button has this id
const IDCANCEL: i32 = 2;

/// The button a result id stands for: ids from 1000 are the buttons in order;
/// `IDCANCEL` and anything else is the cancel button
pub fn index_of(id: i32, buttons: usize, cancel: usize) -> usize {
    usize::try_from(id - FIRST_BUTTON)
        .ok()
        .filter(|i| *i < buttons)
        .unwrap_or(cancel)
}

/// A string as the null-terminated UTF-16 a dialog reads
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn icon_of(kind: Kind) -> PCWSTR {
    match kind {
        Kind::Error => TD_ERROR_ICON,
        Kind::Warning => TD_WARNING_ICON,
        Kind::Info | Kind::Question => TD_INFORMATION_ICON,
        Kind::None => PCWSTR::null(),
    }
}


pub(super) fn show_owned(spec: &Spec, parent: Option<&Window>, session: &Arc<Session>) -> Result<MessageBoxAnswer, String> {
    show_impl(spec, parent, Some(session))
}

fn show_impl(spec: &Spec, parent: Option<&Window>, session: Option<&Arc<Session>>) -> Result<MessageBoxAnswer, String> {
    match parent {
        Some(parent) => parent.native_window_handle().map_err(|error| error.to_string())?
            .with_hwnd(|window| show_with_parent(spec, Some(window), session))
            .map_err(|error| error.to_string())?,
        None => show_with_parent(spec, None, session),
    }
}

fn show_with_parent(spec: &Spec, parent: Option<HWND>, session: Option<&Arc<Session>>) -> Result<MessageBoxAnswer, String> {
    let title = wide(spec.title.as_deref().unwrap_or("SeuOffice"));
    let (instruction, content) = if spec.message.is_empty() {
        (spec.detail.clone().unwrap_or_default(), None)
    } else {
        (spec.message.clone(), spec.detail.clone())
    };
    let instruction = wide(&instruction);
    let content = content.as_deref().map(wide);
    let labels: Vec<Vec<u16>> = spec.buttons.iter().map(|b| wide(b)).collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| TASKDIALOG_BUTTON {
            nButtonID: FIRST_BUTTON + i as i32,
            pszButtonText: PCWSTR(label.as_ptr()),
        })
        .collect();
    let verification = spec.checkbox.as_deref().map(wide);

    let parent_hwnd = parent.unwrap_or(HWND(std::ptr::null_mut::<c_void>()));

    let mut config = TASKDIALOGCONFIG::default();
    config.cbSize = size_of::<TASKDIALOGCONFIG>() as u32;
    config.hwndParent = parent_hwnd;
    config.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION | TDF_SIZE_TO_CONTENT;
    config.pszWindowTitle = PCWSTR(title.as_ptr());
    config.Anonymous1 = TASKDIALOGCONFIG_0 {
        pszMainIcon: icon_of(spec.kind),
    };
    config.pszMainInstruction = PCWSTR(instruction.as_ptr());
    config.pszContent = content
        .as_ref()
        .map_or(PCWSTR::null(), |c| PCWSTR(c.as_ptr()));
    config.cButtons = buttons.len() as u32;
    config.pButtons = buttons.as_ptr();
    config.nDefaultButton = FIRST_BUTTON + spec.effective_default() as i32;
    config.pszVerificationText = verification
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let registration = session.map(task_registration).transpose()?;
    if let Some(session) = session {
        config.pfCallback = Some(task_callback);
        config.lpCallbackData = (session as *const Arc<Session>) as isize;
        session.check()?;
    }

    let mut pressed = 0i32;
    let mut checked = BOOL(0);
    // SAFETY: `config` and every string and the button array it points at live
    // until the call returns; the out parameters are locals
    let result =
        unsafe { TaskDialogIndirect(&config, Some(&mut pressed), None, Some(&mut checked)) };
    drop(registration);
    result.map_err(|error| error.to_string())?;
    Ok(MessageBoxAnswer {
        response: index_of(pressed, spec.buttons.len(), spec.cancel_id),
        checkbox_checked: spec.checkbox.is_some() && checked.as_bool(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ids_start_at_1000_and_cancel_is_the_rest() {
        assert_eq!(index_of(1000, 4, 3), 0);
        assert_eq!(index_of(1003, 4, 0), 3);
        assert_eq!(index_of(IDCANCEL, 4, 3), 3);
        assert_eq!(index_of(1004, 4, 2), 2);
        assert_eq!(wide("ab"), [97, 98, 0]);
    }
}
