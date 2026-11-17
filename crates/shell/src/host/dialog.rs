//! Shared native dialogs retain an exact cancellable native session until it closes.

use std::path::PathBuf;
use std::sync::Arc;

use rfd::{MessageButtons, MessageDialog, MessageLevel};
use shell_contract::hosts::{Dialogs, FileDialog, MessageBox, MessageBoxAnswer};
use shell_contract::ids::WindowId;
use shell_contract::runtime::Thread;
use shell_contract::{DialogsHost, Hosts};
use shell_contract::{ipc::NativeRequestOwner, runtime::CancelToken};
use tauri::{AppHandle, Window};

use super::{HostContext, message_box, on_ui};
use crate::views::ViewTable;

pub(crate) mod owned;

pub struct ShellDialogs {
    app: AppHandle,
    ui: Arc<dyn Thread>,
    views: Arc<ViewTable>,
    effects: shell_contract::hosts::OwnedEffectsHost,
}

pub fn install(hosts: &mut Hosts, cx: &HostContext) {
    let effects = crate::ipc::native_owner::effects_host(cx);
    hosts.insert(effects.clone());
    let dialogs: DialogsHost = Arc::new(ShellDialogs {
        app: cx.app.clone(),
        ui: cx.ui.clone(),
        views: cx.views.clone(),
        effects,
    });
    hosts.insert(dialogs);
}

pub(crate) struct OwnedDialogParent {
    window: Window,
    permit: crate::native::WindowEffect,
    closing: CancelToken,
    _closing_subscription: crate::native::WindowClosingSubscription,
}

fn capture_owned_parent(
    views: &ViewTable,
    parent: Option<WindowId>,
) -> Result<Option<OwnedDialogParent>, String> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    let lifecycle = views
        .window_lifecycle(parent)
        .ok_or("The original dialog parent is unavailable.")?;
    Ok(Some(capture_owned_lifecycle(lifecycle)?))
}

pub(crate) fn capture_owned_lifecycle(
    lifecycle: Arc<crate::views::NativeWindowLifecycle>,
) -> Result<OwnedDialogParent, String> {
    let closing = CancelToken::default();
    let notify = closing.clone();
    let closing_subscription = lifecycle.when_closing(move || notify.cancel())?;
    let permit = lifecycle.admit_native_effect()?;
    let window = lifecycle.window_handle()?;
    Ok(OwnedDialogParent {
        window,
        permit,
        closing,
        _closing_subscription: closing_subscription,
    })
}

pub(crate) fn checked_owned_parent<'a>(
    parent: &'a Option<OwnedDialogParent>,
    session: &Arc<owned::Session>,
) -> Result<Option<&'a Window>, String> {
    match parent {
        Some(parent) if parent.permit.is_current() => {
            session.observe_parent(&parent.closing);
            session.check()?;
            Ok(Some(&parent.window))
        }
        Some(_) => Err("The original dialog parent is closing.".into()),
        None => Ok(None),
    }
}
fn has(properties: &[String], name: &str) -> bool {
    properties.iter().any(|property| property == name)
}

impl Dialogs for ShellDialogs {
    fn open(&self, parent: Option<WindowId>, options: &FileDialog) -> Option<Vec<PathBuf>> {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("shell: open dialog: {error}");
                return None;
            }
        };
        let options = options.clone();
        let app = self.app.clone();
        on_ui(&self.ui, move || -> Result<_, String> {
            let session = owned::Session::standalone()?;
            owned::files(
                &app,
                &options,
                checked_owned_parent(&window, &session)?,
                false,
                &session,
            )
        })
        .unwrap_or_else(|error| {
            eprintln!("shell: open dialog: {error}");
            None
        })
    }

    fn save(&self, parent: Option<WindowId>, options: &FileDialog) -> Option<PathBuf> {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("shell: save dialog: {error}");
                return None;
            }
        };
        let options = options.clone();
        let app = self.app.clone();
        on_ui(&self.ui, move || -> Result<_, String> {
            let session = owned::Session::standalone()?;
            owned::files(
                &app,
                &options,
                checked_owned_parent(&window, &session)?,
                true,
                &session,
            )
            .map(|paths| paths.and_then(|mut paths| paths.pop()))
        })
        .unwrap_or_else(|error| {
            eprintln!("shell: save dialog: {error}");
            None
        })
    }

    fn message_prepared(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        owned::run_prepared(
            self.ui.clone(),
            target,
            snapshot,
            move |session| {
                owned::message(&options, checked_owned_parent(&window, &session)?, &session)
            },
            |answer, _, _| Ok(answer),
            done,
        );
    }

    fn save_prepared(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        done: Box<
            dyn FnOnce(Result<Option<shell_contract::ipc::RecoveryPreparedDestination>, String>)
                + Send,
        >,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        let app = self.app.clone();
        owned::run_prepared(
            self.ui.clone(),
            target,
            snapshot,
            move |session| {
                owned::files(
                    &app,
                    &options,
                    checked_owned_parent(&window, &session)?,
                    true,
                    &session,
                )
                .map(|paths| paths.and_then(|mut paths| paths.pop()))
            },
            |path, target, snapshot| {
                path.map(|path| {
                    shell_contract::ipc::RecoveryPreparedDestination::from_native_save_dialog(
                        target.clone(),
                        snapshot.clone(),
                        path,
                    )
                })
                .transpose()
            },
            done,
        );
    }

    fn message_recovery_owned(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        owned::run_committed(
            self.ui.clone(),
            self.effects.clone(),
            receipt,
            snapshot,
            receiver,
            cancel,
            pending,
            move |session| {
                owned::message(&options, checked_owned_parent(&window, &session)?, &session)
            },
            |answer, _, _| Ok(answer),
            done,
        );
    }

    fn save_recovery_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<
            dyn FnOnce(Result<Option<shell_contract::ipc::RecoveryCommittedDestination>, String>)
                + Send,
        >,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        let app = self.app.clone();
        owned::run_committed(
            self.ui.clone(),
            self.effects.clone(),
            receipt,
            snapshot,
            receiver,
            cancel,
            pending,
            move |session| {
                owned::files(
                    &app,
                    &options,
                    checked_owned_parent(&window, &session)?,
                    true,
                    &session,
                )
                .map(|paths| paths.and_then(|mut paths| paths.pop()))
            },
            |path, receipt, snapshot| {
                path.map(|path| {
                    shell_contract::ipc::RecoveryCommittedDestination::from_native_save_dialog(
                        receipt.clone(),
                        snapshot.clone(),
                        path,
                    )
                })
                .transpose()
            },
            done,
        );
    }

    fn save_existing_document(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        done: Box<
            dyn FnOnce(Result<Option<shell_contract::ipc::NativeSaveDestination>, String>) + Send,
        >,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        let app = self.app.clone();
        owned::run_existing_document(
            self.ui.clone(),
            self.effects.clone(),
            binding,
            participants,
            cancel,
            pending,
            additional,
            move |session| {
                owned::files(
                    &app,
                    &options,
                    checked_owned_parent(&window, &session)?,
                    true,
                    &session,
                )
                .map(|paths| paths.and_then(|mut paths| paths.pop()))
            },
            |path, binding| {
                path.map(|path| {
                    shell_contract::ipc::NativeSaveDestination::from_native_save_dialog(
                        binding.clone(),
                        path,
                    )
                })
                .transpose()
            },
            done,
        );
    }

    fn message_existing_document(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        owned::run_existing_document(
            self.ui.clone(),
            self.effects.clone(),
            binding,
            participants,
            cancel,
            pending,
            additional,
            move |session| {
                owned::message(&options, checked_owned_parent(&window, &session)?, &session)
            },
            |answer, _| Ok(answer),
            done,
        );
    }

    fn save_existing_document_revision(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        snapshot: shell_contract::ipc::NativeSaveRevisionSnapshot,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        done: Box<
            dyn FnOnce(Result<Option<shell_contract::ipc::NativeSaveDestination>, String>) + Send,
        >,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        let app = self.app.clone();
        owned::run_save_revision(
            self.ui.clone(),
            snapshot,
            participants,
            cancel,
            pending,
            additional,
            move |session| {
                owned::files(
                    &app,
                    &options,
                    checked_owned_parent(&window, &session)?,
                    true,
                    &session,
                )
                .map(|paths| paths.and_then(|mut paths| paths.pop()))
            },
            |path, snapshot| {
                path.map(|path| {
                    shell_contract::ipc::NativeSaveDestination::from_native_save_dialog(
                        snapshot.selection().binding().clone(),
                        path,
                    )
                })
                .transpose()
            },
            done,
        );
    }

    fn message_existing_document_revision(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        snapshot: shell_contract::ipc::NativeSaveRevisionSnapshot,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        owned::run_save_revision(
            self.ui.clone(),
            snapshot,
            participants,
            cancel,
            pending,
            additional,
            move |session| {
                owned::message(&options, checked_owned_parent(&window, &session)?, &session)
            },
            |answer, _| Ok(answer),
            done,
        );
    }

    fn open_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<Option<Vec<PathBuf>>, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        let app = self.app.clone();
        owned::run(
            self.ui.clone(),
            origin,
            destination,
            cancel,
            pending,
            move |session| {
                owned::files(
                    &app,
                    &options,
                    checked_owned_parent(&window, &session)?,
                    false,
                    &session,
                )
            },
            done,
        );
    }

    fn save_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<Option<PathBuf>, String>) + Send>,
    ) {
        self.save_owned_many(
            parent,
            options,
            origin,
            vec![destination],
            cancel,
            pending,
            done,
        );
    }

    fn save_owned_many(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<Option<PathBuf>, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        let app = self.app.clone();
        owned::run_many(
            self.ui.clone(),
            origin,
            participants,
            cancel,
            pending,
            move |session| {
                owned::files(
                    &app,
                    &options,
                    checked_owned_parent(&window, &session)?,
                    true,
                    &session,
                )
                .map(|paths| paths.and_then(|mut paths| paths.pop()))
            },
            done,
        );
    }

    fn message_owned(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        self.message_owned_many(
            parent,
            options,
            origin,
            vec![destination],
            cancel,
            pending,
            done,
        );
    }

    fn message_owned_many(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        owned::run_many(
            self.ui.clone(),
            origin,
            participants,
            cancel,
            pending,
            move |session| {
                owned::message(&options, checked_owned_parent(&window, &session)?, &session)
            },
            done,
        );
    }

    fn save_owned_many_extra(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        done: Box<dyn FnOnce(Result<Option<PathBuf>, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        let app = self.app.clone();
        owned::run_many_extra(
            self.ui.clone(),
            origin,
            participants,
            cancel,
            pending,
            additional,
            move |session| {
                owned::files(
                    &app,
                    &options,
                    checked_owned_parent(&window, &session)?,
                    true,
                    &session,
                )
                .map(|paths| paths.and_then(|mut paths| paths.pop()))
            },
            done,
        );
    }

    fn message_owned_many_extra(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => return done(Err(error)),
        };
        owned::run_many_extra(
            self.ui.clone(),
            origin,
            participants,
            cancel,
            pending,
            additional,
            move |session| {
                owned::message(&options, checked_owned_parent(&window, &session)?, &session)
            },
            done,
        );
    }

    fn message(&self, parent: Option<WindowId>, options: &MessageBox) -> MessageBoxAnswer {
        let fallback = MessageBoxAnswer {
            response: message_box::Spec::of(options).cancel_id,
            checkbox_checked: false,
        };
        let window = match capture_owned_parent(&self.views, parent) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("shell: message dialog: {error}");
                return fallback;
            }
        };
        let options = options.clone();
        on_ui(&self.ui, move || -> Result<_, String> {
            let session = owned::Session::standalone()?;
            owned::message(&options, checked_owned_parent(&window, &session)?, &session)
        })
        .unwrap_or_else(|error| {
            eprintln!("shell: message dialog: {error}");
            fallback
        })
    }

    fn error(&self, title: &str, content: &str) {
        if crate::e2e::dialog::intercept_error(title, content) {
            return;
        }
        let (title, content) = (title.to_owned(), content.to_owned());
        on_ui(&self.ui, move || {
            MessageDialog::new()
                .set_level(MessageLevel::Error)
                .set_buttons(MessageButtons::Ok)
                .set_title(title)
                .set_description(content)
                .show();
        });
    }
}
