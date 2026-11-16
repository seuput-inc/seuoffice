//! Shared asynchronous Office document password prompts.
mod owned;
mod strings;
pub(crate) use owned::{
    open_password_window_owned, open_password_window_prepared, open_password_window_saved,
};
mod service;
pub(crate) use service::{DocumentPasswordService, PasswordPrompt};

pub(crate) fn install(hosts: &mut shell_contract::Hosts, cx: &HostContext) {
    hosts.insert(std::sync::Arc::new(DocumentPasswordService::new(cx)));
}

use crate::native::WindowExt;
use shell_contract::Surface;
use shell_contract::ids::{ViewId, WindowId};
use tauri::Manager;

use super::window::{Bridge, ViewSpec, WindowOptions, WindowSpec};
use super::{HostContext, on_ui};

/// Called on a worker, never on the core or UI thread.
pub(crate) fn open_password_window(
    cx: &HostContext,
    parent: Option<WindowId>,
    title: &str,
) -> Option<ViewId> {
    let spec = WindowSpec {
        title: title.to_owned(),
        size: (440.0, 312.0),
        min_size: None,
        visible: false,
        decorations: false,
        transparent: true,
        resizable: false,
        parent,
        focus: false,
        main: false,
        view: ViewSpec {
            url: "genoffice-app://shell/pdf-password.html".into(),
            bridge: Bridge::Surface(Surface::ShellPdfPassword),
        },
    };
    let options = WindowOptions {
        center: true,
        minimizable: false,
        maximizable: false,
        fullscreenable: false,
        ..Default::default()
    };
    let windows = cx.windows.clone();
    let (window, view) = on_ui(&cx.ui, move || {
        windows.open_window_with_options(spec, options)
    })
    .ok()?;
    let child = cx
        .views
        .label_of_window(window)
        .and_then(|label| cx.app.get_window(&label));
    let owner = parent
        .and_then(|id| cx.views.label_of_window(id))
        .and_then(|label| cx.app.get_window(&label));
    let ready = match (child, parent, owner) {
        (Some(child), Some(_), Some(owner)) => child.begin_modal(&owner).is_ok(),
        (Some(_), None, _) => true,
        _ => false,
    };
    if !ready {
        let windows = cx.windows.clone();
        let _ = on_ui(&cx.ui, move || windows.close_window(window));
        return None;
    }
    let windows = cx.windows.clone();
    if on_ui(&cx.ui, move || windows.show(window, true)).is_err() {
        let windows = cx.windows.clone();
        let _ = on_ui(&cx.ui, move || windows.close_window(window));
        return None;
    }
    Some(view)
}
pub(crate) fn unlock_office(
    prompt: crate::host::document_password::PasswordPrompt,
    bytes: &[u8],
) -> Result<Option<(Vec<u8>, String)>, String> {
    use main_core::packages::office_encryption::{OfficeDecryptReason, decrypt_office_file};
    let result = (|| {
        match decrypt_office_file(bytes, "VelvetSweatshop") {
            Ok(plain) => return Ok(Some((plain, "VelvetSweatshop".to_owned()))),
            Err(error) if error.reason == OfficeDecryptReason::WrongPassword => {}
            Err(error) => return Err(error.message),
        }
        let mut retry = false;
        loop {
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            prompt.ask(
                retry,
                Box::new(move |answer| {
                    let _ = send.send(answer);
                }),
            );
            let Some(password) = receive.recv().map_err(|error| error.to_string())?? else {
                return Ok(None);
            };
            if password.is_empty() {
                retry = true;
                continue;
            }
            match decrypt_office_file(bytes, &password) {
                Ok(plain) => return Ok(Some((plain, password))),
                Err(error) if error.reason == OfficeDecryptReason::WrongPassword => retry = true,
                Err(error) => return Err(error.message),
            }
        }
    })();
    prompt.close();
    result
}

/// Called on a worker before the bound recovery view navigates.
pub(crate) fn unlock_prepared(
    service: &DocumentPasswordService,
    target: shell_contract::ipc::RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    title: String,
    bytes: &[u8],
) -> Result<Option<(Vec<u8>, String)>, String> {
    let prompt = service.create_prepared(target.clone(), snapshot.clone(), title)?;
    let answer = unlock_office(prompt, bytes);
    crate::ipc::native_owner::with_recovery_seed_snapshot(&target, snapshot, || answer)
        .ok_or_else(|| "The recovery request expired during document decryption".to_owned())?
}

/// Called on a worker with the saved document's retained committed input.
pub(crate) fn unlock_saved(
    service: &DocumentPasswordService,
    document: shell_contract::ipc::NativeSavedDocument,
    title: String,
) -> Result<Option<(Vec<u8>, String)>, String> {
    let target = document.target();
    let mut input = None;
    crate::ipc::native_owner::with_saved_document_input_effect(
        &document,
        &[],
        target.cancel(),
        target.pending(),
        &[],
        &mut |saved| {
            input = Some(saved.clone());
            Ok(())
        },
    )?;
    let input = input.ok_or("The saved document input was not admitted")?;
    let prompt = service.create_saved(document.clone(), title)?;
    let answer = unlock_office(prompt, input.bytes());
    let mut answer = Some(answer);
    let mut admitted = None;
    crate::ipc::native_owner::with_saved_document_effect(
        &document,
        &[],
        target.cancel(),
        target.pending(),
        &[],
        &mut || {
            admitted = answer.take();
            Ok(())
        },
    )?;
    admitted.ok_or_else(|| "The saved document decryption was not admitted".to_owned())?
}
