//! The dialogs of sheets-main.ts: `showOpenDialogWithMemory`,
//! `showSaveDialogWithMemory`, and the message boxes over the tab's window.

use jscompat::{JsObject, JsonValue};
use main_core::packages::electron_utils::default_save_dir::configured_default_save_dir;
use main_core::packages::electron_utils::dialog_memory::{
    FallbackDir, show_open_dialog_with_memory, show_save_dialog_with_memory,
};
use shell_contract::hosts::MessageBox;
use shell_contract::{IpcError, ViewId, WindowId};

use super::core::Env;
use crate::host::{HostDialog, Paths};
use crate::value::thrown;

/// `hostWindowFor(event.sender)`: the tab's own window, else the shell window
pub fn dialog_parent(env: &Env, view: ViewId) -> Option<WindowId> {
    env.window_of(view).or_else(|| env.main_window.window())
}

/// `{ canceled, filePaths }` of an open dialog
pub struct Picked {
    pub canceled: bool,
    pub paths: Vec<String>,
}

pub fn dialog_options(entries: &[(&str, JsonValue)]) -> JsObject<JsonValue> {
    let mut options = JsObject::new();
    for (key, value) in entries {
        options.set(*key, value.clone());
    }
    options
}

pub fn json_strings(items: &[&str]) -> JsonValue {
    JsonValue::Array(
        items
            .iter()
            .map(|item| JsonValue::String((*item).to_owned()))
            .collect(),
    )
}

pub fn json_filter(name: &str, extensions: &[&str]) -> JsonValue {
    JsonValue::Object(dialog_options(&[
        ("name", JsonValue::String(name.to_owned())),
        ("extensions", json_strings(extensions)),
    ]))
}

/// `openFileDialog(event, options)`
pub fn open_dialog(env: &Env, parent: Option<WindowId>, options: &JsObject<JsonValue>) -> Picked {
    let mut dialog = HostDialog(&*env.dialogs);
    let mut memory = env
        .shared
        .dialog_memory
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let answer =
        show_open_dialog_with_memory(&mut memory, &mut dialog, parent.as_ref(), options, None);
    let get = |key: &str| match &answer {
        JsonValue::Object(object) => object.get(key).cloned(),
        _ => None,
    };
    let paths = match get("filePaths") {
        Some(JsonValue::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                JsonValue::String(text) => Some(text),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Picked {
        canceled: matches!(get("canceled"), Some(JsonValue::Bool(true))),
        paths,
    }
}

/// `{ canceled, filePath }` of a save dialog
pub struct Saved {
    pub canceled: bool,
    pub path: Option<String>,
    /// the native destination the path was normalized to, for a native authority
    pub destination: Option<shell_contract::ipc::NativeSaveDestination>,
}

/// `saveFileDialog(event, options)`: bare names anchor in the default save folder
pub fn save_dialog(env: &Env, parent: Option<WindowId>, options: &JsObject<JsonValue>) -> Saved {
    let mut dialog = HostDialog(&*env.dialogs);
    let mut memory = env
        .shared
        .dialog_memory
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let resolve = || {
        default_save_dir(env)
            .map(Some)
            .map_err(|e| main_core::packages::electron_utils::default_save_dir::Thrown(e.message))
    };
    let answer = show_save_dialog_with_memory(
        &mut memory,
        &mut dialog,
        parent.as_ref(),
        options,
        Some(&FallbackDir::Resolve(&resolve)),
    );
    let get = |key: &str| match &answer {
        JsonValue::Object(object) => object.get(key).cloned(),
        _ => None,
    };
    Saved {
        canceled: matches!(get("canceled"), Some(JsonValue::Bool(true))),
        path: match get("filePath") {
            Some(JsonValue::String(path)) => Some(path),
            _ => None,
        },
        destination: None,
    }
}

/// Worker-side wait for an asynchronously admitted native save dialog.
pub fn save_dialog_owned(
    env: &Env,
    parent: Option<WindowId>,
    options: &JsObject<JsonValue>,
    scope: &crate::owned_save::OwnedSave,
) -> Result<Saved, IpcError> {
    save_dialog_owned_with_fallback(env, parent, options, scope, true)
}

pub fn save_dialog_scoped(
    env: &Env,
    parent: Option<WindowId>,
    options: &JsObject<JsonValue>,
    scope: Option<&crate::owned_save::OwnedSave>,
    fallback: bool,
) -> Result<Saved, IpcError> {
    match scope {
        Some(scope) => save_dialog_owned_with_fallback(env, parent, options, scope, fallback),
        None if fallback => Ok(save_dialog(env, parent, options)),
        None => Ok(save_dialog_plain(env, parent, options)),
    }
}

fn save_dialog_owned_with_fallback(
    env: &Env,
    parent: Option<WindowId>,
    options: &JsObject<JsonValue>,
    scope: &crate::owned_save::OwnedSave,
    fallback: bool,
) -> Result<Saved, IpcError> {
    scope.check()?;
    let resolve = || {
        default_save_dir(env).map(Some).map_err(|error| {
            main_core::packages::electron_utils::default_save_dir::Thrown(error.message)
        })
    };
    let prepared = {
        let memory = env
            .shared
            .dialog_memory
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        main_core::packages::electron_utils::dialog_memory::prepare_dialog_options(
            &memory,
            options,
            if fallback {
                Some(&FallbackDir::Resolve(&resolve))
            } else {
                None
            },
        )
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    scope.save_dialog(
        &env.dialogs,
        parent,
        crate::host::file_dialog(&prepared),
        Box::new(move |result| {
            let _ = tx.send(result);
        }),
    );
    let picked = rx
        .recv()
        .map_err(|_| thrown("The native save dialog did not complete"))?
        .map_err(thrown)?;
    scope.check()?;
    if let Some(path) = &picked {
        scope.effect(|| {
            let mut memory = env
                .shared
                .dialog_memory
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            main_core::packages::electron_utils::dialog_memory::remember_save_path(
                &mut memory,
                &crate::host::path_string(path.path().to_path_buf()),
            );
            Ok(())
        })?;
    }
    let canceled = picked.is_none();
    let (path, destination) = match picked {
        Some(crate::owned_save::SaveDestination::Renderer(path)) => {
            (Some(crate::host::path_string(path)), None)
        }
        Some(crate::owned_save::SaveDestination::Native(destination)) => (
            Some(crate::host::path_string(destination.path().to_path_buf())),
            Some(destination),
        ),
        None => (None, None),
    };
    Ok(Saved {
        canceled,
        path,
        destination,
    })
}

pub fn ask_owned(
    env: &Env,
    parent: Option<WindowId>,
    options: &MessageBox,
    scope: &crate::owned_save::OwnedSave,
) -> Result<usize, IpcError> {
    scope.check()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    scope.message_dialog(
        &env.dialogs,
        parent,
        options.clone(),
        Box::new(move |result| {
            let _ = tx.send(result.map(|answer| answer.response));
        }),
    );
    let answer = rx
        .recv()
        .map_err(|_| thrown("The native message dialog did not complete"))?
        .map_err(thrown)?;
    scope.check()?;
    Ok(answer)
}

pub struct OwnedMessages<'a> {
    pub env: &'a Env,
    pub scope: &'a crate::owned_save::OwnedSave,
    pub error: std::cell::RefCell<Option<IpcError>>,
}

impl main_core::packages::electron_utils::message_box::MessageBox<Win> for OwnedMessages<'_> {
    fn show_message_box_over(
        &self,
        parent: &Win,
        options: &main_core::packages::electron_utils::message_box::MessageBoxOptions,
    ) -> f64 {
        self.show(Some(parent.0), options)
    }
    fn show_message_box(
        &self,
        options: &main_core::packages::electron_utils::message_box::MessageBoxOptions,
    ) -> f64 {
        self.show(None, options)
    }
}

impl OwnedMessages<'_> {
    fn show(
        &self,
        parent: Option<WindowId>,
        options: &main_core::packages::electron_utils::message_box::MessageBoxOptions,
    ) -> f64 {
        match ask_owned(self.env, parent, &contract_box(options), self.scope) {
            Ok(answer) => answer as f64,
            Err(error) => {
                *self.error.borrow_mut() = Some(error);
                options.cancel_id.map_or(-1.0, f64::from)
            }
        }
    }
}

/// `showSaveDialogWithMemory(dialog, parent, options)` without a fallback folder
/// (`exportPdf` of pdf-export.ts)
pub fn save_dialog_plain(
    env: &Env,
    parent: Option<WindowId>,
    options: &JsObject<JsonValue>,
) -> Saved {
    let mut dialog = HostDialog(&*env.dialogs);
    let mut memory = env
        .shared
        .dialog_memory
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let answer =
        show_save_dialog_with_memory(&mut memory, &mut dialog, parent.as_ref(), options, None);
    let get = |key: &str| match &answer {
        JsonValue::Object(object) => object.get(key).cloned(),
        _ => None,
    };
    Saved {
        canceled: matches!(get("canceled"), Some(JsonValue::Bool(true))),
        path: match get("filePath") {
            Some(JsonValue::String(path)) => Some(path),
            _ => None,
        },
        destination: None,
    }
}

/// `configuredDefaultSaveDir(app)`
pub fn default_save_dir(env: &Env) -> Result<String, IpcError> {
    let mut state = env
        .shared
        .save_dir
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    configured_default_save_dir(&Paths(&*env.services.paths), &mut state, env.shell.now_ms())
        .map_err(|error| thrown(error.0))
}

/// A message box literal: `{ type, message, detail?, buttons, defaultId, cancelId? }`
pub fn message_box(
    kind: &str,
    message: String,
    detail: Option<String>,
    buttons: Vec<String>,
    default_id: usize,
    cancel_id: Option<usize>,
) -> MessageBox {
    MessageBox {
        kind: Some(kind.to_owned()),
        title: None,
        message,
        detail,
        buttons,
        default_id: Some(default_id),
        cancel_id,
        checkbox_label: None,
    }
}

/// `dialog.showMessageBox(parent, options)` or without a parent: the pressed button
pub fn ask(env: &Env, parent: Option<WindowId>, options: &MessageBox) -> usize {
    env.dialogs.message(parent, options).response
}

/// A window the tab's dialogs sit over; a window the shell no longer has is
/// not offered as a parent
pub struct Win(pub WindowId);

impl main_core::packages::electron_utils::message_box::ParentWindow for Win {
    fn is_destroyed(&self) -> bool {
        false
    }
}

/// `dialog.showMessageBox` for `main-core`'s modules over the shell's [`Dialogs`] host
pub struct HostMessages<'a>(pub &'a dyn shell_contract::hosts::Dialogs);

impl main_core::packages::electron_utils::message_box::MessageBox<Win> for HostMessages<'_> {
    fn show_message_box_over(
        &self,
        parent: &Win,
        options: &main_core::packages::electron_utils::message_box::MessageBoxOptions,
    ) -> f64 {
        self.0
            .message(Some(parent.0), &contract_box(options))
            .response as f64
    }

    fn show_message_box(
        &self,
        options: &main_core::packages::electron_utils::message_box::MessageBoxOptions,
    ) -> f64 {
        self.0.message(None, &contract_box(options)).response as f64
    }
}

fn contract_box(
    options: &main_core::packages::electron_utils::message_box::MessageBoxOptions,
) -> MessageBox {
    MessageBox {
        kind: Some(options.kind.to_owned()),
        title: options.title.clone(),
        message: options.message.clone(),
        detail: options.detail.clone(),
        buttons: options.buttons.clone(),
        default_id: Some(options.default_id as usize),
        cancel_id: options.cancel_id.map(|id| id as usize),
        checkbox_label: None,
    }
}
