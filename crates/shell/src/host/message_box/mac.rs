//! The message box on macOS: `NSAlert`, set up as Electron's
//! `shell/browser/ui/message_box_mac.mm` sets it up. Buttons are added in
//! order and answer `NSAlertFirstButtonReturn + index`. Electron does not
//! reorder them: it moves Return (key equivalent `\r`) to `defaultId` and
//! gives Escape (`\033`) to `cancelId`. The checkbox is the alert's
//! suppression button. With a parent window the alert is a sheet; the call
//! still waits for it in a modal session, as `dialog.showMessageBoxSync` does.

use crate::host::dialog::owned::macos::with_ns_window;
use crate::host::dialog::owned::{Session, macos::register_alert};
use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSApplication, NSControlStateValueOff, NSControlStateValueOn,
    NSModalResponse, NSWindow,
};
use objc2_foundation::NSString;
use shell_contract::hosts::MessageBoxAnswer;
use std::sync::Arc;
use tauri::Window;

use super::{Kind, Spec};

/// `NSAlertFirstButtonReturn`: the buttons answer 1000, 1001, ...
const FIRST_BUTTON: isize = 1000;

/// The index a button's response stands for; a response that is none of the
/// buttons (a dismissed alert) is the cancel button
pub fn index_of(response: isize, buttons: usize, cancel: usize) -> usize {
    usize::try_from(response - FIRST_BUTTON)
        .ok()
        .filter(|i| *i < buttons)
        .unwrap_or(cancel)
}

fn style_of(kind: Kind) -> NSAlertStyle {
    match kind {
        Kind::Warning => NSAlertStyle::Warning,
        Kind::Error => NSAlertStyle::Critical,
        _ => NSAlertStyle::Informational,
    }
}

pub(super) fn show_owned(
    spec: &Spec,
    parent: Option<&Window>,
    session: &Arc<Session>,
) -> Result<MessageBoxAnswer, String> {
    show_impl(spec, parent, Some(session))
}

fn show_impl(
    spec: &Spec,
    parent: Option<&Window>,
    session: Option<&Arc<Session>>,
) -> Result<MessageBoxAnswer, String> {
    match parent {
        Some(parent) => with_ns_window(
            &crate::native::WindowExt::native_window_handle(parent)?,
            parent,
            |window| show_with_parent(spec, Some(window), session),
        )?,
        None => show_with_parent(spec, None, session),
    }
}

fn show_with_parent(
    spec: &Spec,
    parent_window: Option<&NSWindow>,
    session: Option<&Arc<Session>>,
) -> Result<MessageBoxAnswer, String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;

    let alert = NSAlert::new(mtm);
    // the title is not shown by an NSAlert, as in Electron
    alert.setMessageText(&NSString::from_str(if spec.message.is_empty() {
        spec.detail.as_deref().unwrap_or("")
    } else {
        &spec.message
    }));
    if !spec.message.is_empty()
        && let Some(detail) = &spec.detail
    {
        alert.setInformativeText(&NSString::from_str(detail));
    }
    alert.setAlertStyle(style_of(spec.kind));

    let buttons: Vec<_> = spec
        .buttons
        .iter()
        .map(|title| alert.addButtonWithTitle(&NSString::from_str(title)))
        .collect();
    // the first button added is the default: move Return to `defaultId`
    let default = spec.effective_default();
    if default != 0 {
        buttons[0].setKeyEquivalent(&NSString::from_str(""));
        buttons[default].setKeyEquivalent(&NSString::from_str("\r"));
    }
    if spec.cancel_id != default {
        buttons[spec.cancel_id].setKeyEquivalent(&NSString::from_str("\u{1b}"));
    }

    if let Some(label) = &spec.checkbox {
        alert.setShowsSuppressionButton(true);
        if let Some(check) = alert.suppressionButton() {
            check.setTitle(&NSString::from_str(label));
            check.setState(NSControlStateValueOff);
        }
    }

    let registration = session
        .map(|session| register_alert(session, &alert))
        .transpose()?;
    if let Some(session) = session {
        session.check()?;
    }
    let response: NSModalResponse = match &parent_window {
        Some(window) => {
            let app = NSApplication::sharedApplication(mtm);
            let end = {
                let app = app.clone();
                let owned = session.map(|session| (session.id, alert.window()));
                RcBlock::new(move |code: NSModalResponse| {
                    if let Some((id, exact)) = &owned {
                        if !crate::host::dialog::owned::macos::alert_is_registered(*id, exact)
                            || !app
                                .modalWindow()
                                .as_deref()
                                .is_some_and(|modal| std::ptr::eq(modal, &**exact))
                        {
                            return;
                        }
                    }
                    app.stopModalWithCode(code);
                })
            };
            alert.beginSheetModalForWindow_completionHandler(window, Some(&end));
            if session.is_some_and(|session| session.cancelled()) {
                0
            } else {
                app.runModalForWindow(&alert.window())
            }
        }
        None => alert.runModal(),
    };
    if session.is_some() {
        let window = alert.window();
        if let Some(parent) = window.sheetParent() {
            parent.endSheet_returnCode(&window, response);
        }
        window.orderOut(None);
    }
    drop(registration);

    let checked = alert
        .suppressionButton()
        .is_some_and(|b| b.state() == NSControlStateValueOn);
    Ok(MessageBoxAnswer {
        response: index_of(response, spec.buttons.len(), spec.cancel_id),
        checkbox_checked: spec.checkbox.is_some() && checked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buttons_answer_from_1000_and_anything_else_is_cancel() {
        assert_eq!(index_of(1000, 3, 2), 0);
        assert_eq!(index_of(1002, 3, 2), 2);
        assert_eq!(index_of(1003, 3, 1), 1);
        assert_eq!(index_of(0, 3, 1), 1);
        assert_eq!(index_of(-1000, 3, 1), 1);
    }
}
