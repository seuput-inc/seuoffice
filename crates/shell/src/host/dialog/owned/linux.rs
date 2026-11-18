use std::{cell::{Cell, RefCell}, path::PathBuf, rc::Rc, sync::Arc};
use gtk4::{gio, glib, prelude::*};
use shell_contract::hosts::MessageBoxAnswer;
use crate::host::message_box::{Kind, Spec};
use super::{FileSpec, NativeSessions, Session};
use tauri_runtime_cef::WindowCefExt;

thread_local! {
    static ACTIVE: RefCell<NativeSessions<gtk4::Dialog>> = RefCell::new(NativeSessions::new());
    static DIRECTORIES: RefCell<[Option<PathBuf>; 3]> = const { RefCell::new([None, None, None]) };
}
struct Registration(u64);
impl Drop for Registration {
    fn drop(&mut self) {
        let removed = ACTIVE.with(|active| active.borrow_mut().remove(&self.0));
        drop(removed);
    }
}

pub(super) fn prepare(session: &Arc<Session>) -> Result<(), String> {
    if !gtk4::is_initialized_main_thread() { return Err("Native dialog is not on the GTK main thread.".into()); }
    session.check()
}

pub(super) fn wake(session: &Session) {
    let id = session.id;
    glib::MainContext::default().invoke(move || {
        let dialog = ACTIVE.with(|active| active.borrow().get(&id).cloned());
        if let Some(dialog) = dialog { dialog.response(gtk4::ResponseType::Cancel); }
    });
}

fn wait<T: 'static>(dialog: &gtk4::Dialog, session: &Arc<Session>, read: impl Fn(&gtk4::Dialog, gtk4::ResponseType) -> Result<T, String> + 'static) -> Result<T, String> {
    let result = Rc::new(RefCell::new(None));
    let completed = Cell::new(false);
    let main_loop = glib::MainLoop::new(None, false);
    let answered = result.clone();
    let stop = main_loop.clone();
    let id = session.id;
    dialog.connect_response(move |dialog, response| {
        if completed.replace(true) { return; }
        *answered.borrow_mut() = Some(read(dialog, response));
        let removed = ACTIVE.with(|active| active.borrow_mut().remove(&id));
        drop(removed);
        dialog.hide();
        dialog.destroy();
        stop.quit();
    });
    dialog.connect_close_request(|dialog| {
        dialog.response(gtk4::ResponseType::Cancel);
        glib::Propagation::Stop
    });
    if let Err(error) = ACTIVE.with(|active| active.borrow_mut().insert(id, dialog.clone())) {
        dialog.destroy();
        return Err(error);
    }
    let registration = Registration(id);
    if let Err(error) = session.check() { dialog.destroy(); return Err(error); }
    dialog.present();
    if result.borrow().is_none() { main_loop.run(); }
    drop(registration);
    let result = result.borrow_mut().take().ok_or("Native GTK dialog closed without a response.")?;
    result
}

pub(super) fn files(_app: &tauri::AppHandle, spec: &FileSpec, parent: Option<&tauri::Window>, save: bool, session: &Arc<Session>) -> Result<Option<Vec<PathBuf>>, String> {
    match parent {
        Some(parent) => parent.native_window_handle().map_err(|error| error.to_string())?
            .with_gtk_window(|window| files_with_parent(spec, Some(window), save, session))
            .map_err(|error| error.to_string())?,
        None => files_with_parent(spec, None, save, session),
    }
}

fn files_with_parent(spec: &FileSpec, parent: Option<&gtk4::ApplicationWindow>, save: bool, session: &Arc<Session>) -> Result<Option<Vec<PathBuf>>, String> {
    let action = if save { gtk4::FileChooserAction::Save } else if spec.folders { gtk4::FileChooserAction::SelectFolder } else { gtk4::FileChooserAction::Open };
    let dialog = gtk4::FileChooserDialog::new(spec.title.as_deref(), parent, action,
        &[("Cancel", gtk4::ResponseType::Cancel), (if save { "Save" } else { "Open" }, gtk4::ResponseType::Accept)]);
    dialog.set_modal(true);
    dialog.set_select_multiple(!save && spec.multiple);
    dialog.set_create_folders(spec.create_directories.unwrap_or(true));
    dialog.set_default_response(gtk4::ResponseType::Accept);
    for (name, extensions) in &spec.filters {
        let filter = gtk4::FileFilter::new();
        filter.set_name(Some(name));
        for extension in extensions { filter.add_pattern(&format!("*.{extension}")); }
        dialog.add_filter(&filter);
    }
    let slot = if save { 0 } else if spec.folders { 1 } else { 2 };
    let directory = spec.directory.clone().or_else(|| DIRECTORIES.with(|directories| directories.borrow()[slot].clone()));
    if let Some(directory) = directory {
        if let Err(error) = dialog.set_current_folder(Some(&gio::File::for_path(directory))) {
            dialog.destroy();
            return Err(error.to_string());
        }
    }
    if save && let Some(name) = &spec.name { dialog.set_current_name(name); }
    wait(dialog.upcast_ref(), session, move |dialog, response| {
        let chooser = dialog.downcast_ref::<gtk4::FileChooserDialog>().ok_or("Native GTK chooser identity changed.")?;
        let directory = chooser.current_folder().and_then(|folder| folder.path());
        DIRECTORIES.with(|directories| { directories.borrow_mut()[slot] = directory; });
        if response != gtk4::ResponseType::Accept { return Ok(None); }
        let files = chooser.files();
        let mut paths = Vec::with_capacity(files.n_items() as usize);
        for index in 0..files.n_items() {
            let file = files.item(index).and_downcast::<gio::File>().ok_or("Native GTK chooser returned an invalid file.")?;
            paths.push(file.path().ok_or("Native GTK chooser returned a non-filesystem URL.")?);
        }
        Ok(Some(paths))
    })
}

pub(crate) fn message(spec: &Spec, parent: Option<&tauri::Window>, session: &Arc<Session>) -> Result<MessageBoxAnswer, String> {
    match parent {
        Some(parent) => parent.native_window_handle().map_err(|error| error.to_string())?
            .with_gtk_window(|window| message_with_parent(spec, Some(window), session))
            .map_err(|error| error.to_string())?,
        None => message_with_parent(spec, None, session),
    }
}

fn message_with_parent(spec: &Spec, parent: Option<&gtk4::ApplicationWindow>, session: &Arc<Session>) -> Result<MessageBoxAnswer, String> {
    let kind = match spec.kind { Kind::Error => gtk4::MessageType::Error, Kind::Warning => gtk4::MessageType::Warning, _ => gtk4::MessageType::Info };
    let dialog = gtk4::MessageDialog::builder().modal(true).message_type(kind).text(&spec.message).build();
    dialog.set_use_markup(false);
    dialog.set_secondary_use_markup(false);
    dialog.set_secondary_text(spec.detail.as_deref());
    dialog.set_transient_for(parent);
    if let Some(title) = &spec.title { dialog.set_title(Some(title)); }
    for (index, label) in spec.buttons.iter().enumerate() {
        let index = u16::try_from(index).map_err(|_| "Native GTK message box has too many buttons.")?;
        dialog.add_button(label, gtk4::ResponseType::Other(index));
    }
    let default = u16::try_from(spec.effective_default()).map_err(|_| "Native GTK message box default is invalid.")?;
    dialog.set_default_response(gtk4::ResponseType::Other(default));
    let checkbox = spec.checkbox.as_ref().map(|label| {
        let checkbox = gtk4::CheckButton::with_label(label);
        dialog.content_area().append(&checkbox);
        checkbox
    });
    let buttons = spec.buttons.clone();
    let cancel = spec.cancel_id;
    wait(dialog.upcast_ref(), session, move |_, response| {
        let response = match response {
            gtk4::ResponseType::Other(index) => buttons.get(usize::from(index))
                .and_then(|selected| buttons.iter().position(|label| label == selected)).unwrap_or(cancel),
            _ => cancel,
        };
        Ok(MessageBoxAnswer { response, checkbox_checked: checkbox.as_ref().is_some_and(|checkbox| checkbox.is_active()) })
    })
}
