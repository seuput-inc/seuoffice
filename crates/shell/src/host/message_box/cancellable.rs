//! Request-owned GTK4 message boxes. No native lock is held while a box is open.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}, mpsc};

use gtk4::prelude::*;
use shell_contract::{hosts::{MessageBox, MessageBoxAnswer}, ipc::NativeRequestOwner, runtime::{CancelToken, Views}};
use tauri_runtime_cef::WindowCefExt;
use crate::host::HostContext;
use super::{Kind, Spec};

pub(crate) trait DialogAuthority: Send + Sync {
    fn owner_loss(&self, owner: &NativeRequestOwner) -> Option<CancelToken>;
    fn is_current(&self, owner: &NativeRequestOwner, cancel: &CancelToken) -> bool;
    fn with_admission(&self, owner: &NativeRequestOwner, cancel: &CancelToken, action: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String>;
}

pub(crate) struct CancellableDialogs {
    cx: HostContext,
    authority: Arc<dyn DialogAuthority>,
}

thread_local! {
    static ACTIVE: RefCell<HashMap<u64, gtk4::MessageDialog>> = RefCell::new(HashMap::new());
}
static NEXT: AtomicU64 = AtomicU64::new(1);

fn dismiss(id: u64) {
    let dialog = ACTIVE.with(|active| active.borrow().get(&id).cloned());
    if let Some(dialog) = dialog { dialog.response(gtk4::ResponseType::Cancel); }
}

impl CancellableDialogs {
    pub(crate) fn new(cx: HostContext, authority: Arc<dyn DialogAuthority>) -> Self { Self { cx, authority } }

    /// Called on an interaction worker, never on the UI or core thread.
    pub(crate) fn show_cancellable(&self, owner: &NativeRequestOwner, options: &MessageBox, request_cancel: &CancelToken, entry_cancel: &CancelToken) -> Result<MessageBoxAnswer, String> {
        if self.cx.ui.is_current() { return Err("certificate dialog called on UI thread".into()); }
        let owner_loss = self.authority.owner_loss(owner).ok_or("certificate dialog owner unavailable")?;
        if request_cancel.is_cancelled() || entry_cancel.is_cancelled() || owner_loss.is_cancelled() || !self.authority.is_current(owner, request_cancel) {
            return Err("certificate dialog owner expired".into());
        }
        let id = NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1)).map_err(|_| "certificate dialog identifiers exhausted")?;
        let (tx, rx) = mpsc::channel();
        let ui = self.cx.ui.clone();
        let subscribe = |token: &CancelToken| {
            let ui = ui.clone();
            token.on_cancel(move || ui.post(Box::new(move || dismiss(id))))
        };
        let generation = owner.context.renderer().ok_or("certificate dialog renderer unavailable")?;
        let (_, lifecycle) = self.cx.views.view_window_captured(owner.view, generation)?;
        let parent = crate::host::dialog::capture_owned_lifecycle(lifecycle)?;
        let _subscriptions = [subscribe(request_cancel), subscribe(entry_cancel), subscribe(&owner_loss), subscribe(parent.closing())];
        let (authority, owner, request, entry, loss, spec) = (
            self.authority.clone(), owner.clone(), request_cancel.clone(), entry_cancel.clone(), owner_loss.clone(), Spec::of(options),
        );
        ui.post(Box::new(move || {
            let sender = Rc::new(RefCell::new(Some(tx)));
            let result = (|| -> Result<(), String> {
                if request.is_cancelled() || entry.is_cancelled() || loss.is_cancelled() || !authority.is_current(&owner, &request) {
                    return Err("certificate dialog owner expired".into());
                }
                let handle = parent.window()?.native_window_handle().map_err(|error| error.to_string())?;
                let parent = Rc::new(parent);
                handle.with_gtk_window(|native| -> Result<(), String> {
                    let kind = match spec.kind { Kind::Error => gtk4::MessageType::Error, Kind::Warning => gtk4::MessageType::Warning, _ => gtk4::MessageType::Info };
                    let dialog = gtk4::MessageDialog::builder().transient_for(native).modal(true).message_type(kind).text(spec.message.as_str()).build();
                    dialog.set_use_markup(false);
                    dialog.set_secondary_use_markup(false);
                    dialog.set_secondary_text(spec.detail.as_deref());
                    if let Some(title) = &spec.title { dialog.set_title(Some(title)); }
                    for (index, label) in spec.buttons.iter().enumerate() {
                        let index = u16::try_from(index).map_err(|_| "certificate dialog has too many buttons")?;
                        dialog.add_button(label, gtk4::ResponseType::Other(index));
                    }
                    let default = u16::try_from(spec.effective_default()).map_err(|_| "certificate dialog default is invalid")?;
                    dialog.set_default_response(gtk4::ResponseType::Other(default));
                    let checkbox = spec.checkbox.as_ref().map(|label| {
                        let button = gtk4::CheckButton::with_label(label);
                        dialog.content_area().append(&button);
                        button
                    });
                    let response_sender = sender.clone();
                    let response_parent = parent.clone();
                    let cancel_index = spec.cancel_id;
                    dialog.connect_response(move |dialog, response| {
                        let _parent = &response_parent;
                        let index = match response { gtk4::ResponseType::Other(index) if usize::from(index) < spec.buttons.len() => usize::from(index), _ => cancel_index };
                        let checked = checkbox.as_ref().is_some_and(|button| button.is_active());
                        let sender = response_sender.borrow_mut().take();
                        ACTIVE.with(|active| { active.borrow_mut().remove(&id); });
                        dialog.destroy();
                        if let Some(sender) = sender { let _ = sender.send(Ok(MessageBoxAnswer { response: index, checkbox_checked: checked })); }
                    });
                    let close_sender = sender.clone();
                    let close_parent = parent.clone();
                    dialog.connect_close_request(move |window| {
                        let _parent = &close_parent;
                        let sender = close_sender.borrow_mut().take();
                        ACTIVE.with(|active| { active.borrow_mut().remove(&id); });
                        window.destroy();
                        if let Some(sender) = sender { let _ = sender.send(Ok(MessageBoxAnswer { response: cancel_index, checkbox_checked: false })); }
                        gtk4::glib::Propagation::Stop
                    });
                    ACTIVE.with(|active| { active.borrow_mut().insert(id, dialog.clone()); });
                    let mut accepted = None;
                    authority.with_admission(&owner, &request, &mut || {
                        if request.is_cancelled() || entry.is_cancelled() || loss.is_cancelled() || parent.closing().is_cancelled() {
                            return Err("certificate dialog owner expired".into());
                        }
                        accepted = Some(());
                        Ok(())
                    })?;
                    if accepted.is_none() { return Err("certificate dialog was not admitted".into()); }
                    parent.window()?;
                    dialog.present();
                    Ok(())
                }).map_err(|error| error.to_string())?
            })();
            if let Err(error) = result {
                let dialog = ACTIVE.with(|active| active.borrow_mut().remove(&id));
                if let Some(dialog) = dialog { dialog.destroy(); }
                let sender = sender.borrow_mut().take();
                if let Some(sender) = sender { let _ = sender.send(Err(error)); }
            }
        }));
        let answer = rx.recv().map_err(|_| "certificate dialog closed unexpectedly".to_owned())??;
        if request_cancel.is_cancelled() || entry_cancel.is_cancelled() || owner_loss.is_cancelled() || !self.authority.is_current(owner, request_cancel) {
            return Err("certificate dialog owner expired".into());
        }
        self.authority.with_admission(owner, request_cancel, &mut || {
            if entry_cancel.is_cancelled() || owner_loss.is_cancelled() { Err("certificate dialog owner expired".into()) } else { Ok(()) }
        })?;
        Ok(answer)
    }
}
