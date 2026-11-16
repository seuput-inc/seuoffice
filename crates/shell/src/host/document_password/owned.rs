use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::native::{DocumentContext, NativeEventKind, WindowExt};
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelSubscription, CancelToken, RendererGeneration, Views};
use shell_contract::{Surface, ViewId, WindowId};
use shell_ipc_d2::pdf::tools::OwnedPasswordWindowActions;
use tauri::Manager;

use crate::host::HostContext;
use crate::host::window::{Bridge, ViewSpec, WindowOptions, WindowSpec};
use crate::ipc::native_owner;

type Ready = Box<dyn FnOnce(Result<(ViewId, NativeRequestOwner), String>) + Send>;

enum Authority {
    Renderer {
        source: NativeRequestOwner,
        destination: NativeRequestOwner,
    },
    Prepared {
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    },
    Saved {
        document: Box<shell_contract::ipc::NativeSavedDocument>,
        parent: Box<tauri::Window>,
    },
}

struct Created {
    view: ViewId,
    generation: RendererGeneration,
    native: tauri::Window,
    parent: Option<tauri::Window>,
}

struct Opening {
    cx: HostContext,
    authority: Authority,
    cancel: CancelToken,
    pending: CancelToken,
    created: OnceLock<Created>,
    owner: Mutex<Option<NativeRequestOwner>>,
    done: Mutex<Option<Ready>>,
    losses: Mutex<Vec<CancelSubscription>>,
    closed: AtomicBool,
    cleaned: AtomicBool,
    shown: AtomicBool,
}

impl Opening {
    fn saved_effect<R>(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        created: Option<&RendererGeneration>,
        owner: Option<&NativeRequestOwner>,
        action: impl FnOnce() -> R,
    ) -> Result<R, String> {
        let mut action = Some(action);
        let mut result = None;
        native_owner::with_saved_document_dialog_effect(
            document,
            created,
            owner,
            &self.cancel,
            &self.pending,
            &mut || {
                result = Some(action.take().expect("saved password admission runs once")());
                Ok(())
            },
        )?;
        result.ok_or_else(|| "The saved password operation was not admitted".into())
    }

    fn saved_ui<R>(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        created: Option<&RendererGeneration>,
        owner: Option<&NativeRequestOwner>,
        action: impl FnOnce() -> R,
    ) -> Result<R, String> {
        if !self.cx.ui.is_current() {
            return Err("The password window operation requires the UI thread".into());
        }
        self.saved_effect(document, created, owner, || ())?;
        Ok(action())
    }

    fn seed_ui<R>(&self, action: impl FnOnce() -> R) -> Result<R, String> {
        match &self.authority {
            Authority::Renderer {
                source,
                destination,
            } => native_owner::run_accepted_ui_pair(
                self.cx.ui.as_ref(),
                source,
                destination,
                &self.cancel,
                &self.pending,
                action,
            ),
            Authority::Prepared { target, snapshot } => native_owner::run_recovery_snapshot_ui(
                self.cx.ui.as_ref(),
                target,
                snapshot,
                None,
                None,
                Some((&self.cancel, &self.pending)),
                action,
            ),
            Authority::Saved { document, .. } => self.saved_ui(document, None, None, action),
        }
    }

    fn created_ui<R>(
        &self,
        created: &RendererGeneration,
        action: impl FnOnce() -> R,
    ) -> Result<R, String> {
        match &self.authority {
            Authority::Renderer {
                source,
                destination,
            } => native_owner::run_accepted_created_ui_many(
                self.cx.ui.as_ref(),
                source,
                std::slice::from_ref(destination),
                created,
                &self.cancel,
                &self.pending,
                action,
            ),
            Authority::Prepared { target, snapshot } => native_owner::run_recovery_snapshot_ui(
                self.cx.ui.as_ref(),
                target,
                snapshot,
                Some(created),
                None,
                Some((&self.cancel, &self.pending)),
                action,
            ),
            Authority::Saved { document, .. } => {
                self.saved_ui(document, Some(created), None, action)
            }
        }
    }

    fn dialog_effect<R>(
        &self,
        owner: &NativeRequestOwner,
        created: &RendererGeneration,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        match &self.authority {
            Authority::Renderer {
                source,
                destination,
            } => native_owner::with_many_owner_transition(
                source,
                &[destination.clone(), owner.clone()],
                &self.cancel,
                &self.pending,
                |_| action(),
            ),
            Authority::Prepared { target, snapshot } => {
                native_owner::with_recovery_pre_navigation_snapshot(
                    target,
                    snapshot,
                    Some(created),
                    Some(owner),
                    Some((&self.cancel, &self.pending)),
                    action,
                )
            }
            Authority::Saved { document, .. } => self
                .saved_effect(document, Some(created), Some(owner), action)
                .ok(),
        }
    }

    fn dialog_ui<R>(
        &self,
        owner: &NativeRequestOwner,
        created: &RendererGeneration,
        action: impl FnOnce() -> R,
    ) -> Result<R, String> {
        match &self.authority {
            Authority::Renderer {
                source,
                destination,
            } => native_owner::run_accepted_ui_many(
                self.cx.ui.as_ref(),
                source,
                &[destination.clone(), owner.clone()],
                &self.cancel,
                &self.pending,
                action,
            ),
            Authority::Prepared { target, snapshot } => native_owner::run_recovery_snapshot_ui(
                self.cx.ui.as_ref(),
                target,
                snapshot,
                Some(created),
                Some(owner),
                Some((&self.cancel, &self.pending)),
                action,
            ),
            Authority::Saved { document, .. } => {
                self.saved_ui(document, Some(created), Some(owner), action)
            }
        }
    }

    fn loss_tokens(&self) -> Result<Vec<CancelToken>, String> {
        let mut tokens = vec![self.cancel.clone(), self.pending.clone()];
        match &self.authority {
            Authority::Renderer {
                source,
                destination,
            } => {
                tokens.push(
                    source
                        .loss_token()
                        .ok_or("The password source owner lease is unavailable")?,
                );
                tokens.push(
                    destination
                        .loss_token()
                        .ok_or("The password destination owner lease is unavailable")?,
                );
            }
            Authority::Prepared { snapshot, .. } => {
                tokens.extend(snapshot.preparation_loss_tokens()?)
            }
            Authority::Saved { document, .. } => {
                let target = document.target();
                let output = target.output();
                tokens.extend([target.cancel().clone(), target.pending().clone()]);
                tokens.extend(output.loss_tokens()?);
                tokens.extend_from_slice(target.additional());
                for owner in std::iter::once(output.source_owner())
                    .chain(target.participants().iter())
                    .chain(std::iter::once(document.owner()))
                {
                    tokens.push(
                        owner
                            .loss_token()
                            .ok_or("The saved password owner lease is unavailable")?,
                    );
                }
                tokens.push(target.created().loss_token());
            }
        }
        Ok(tokens)
    }

    fn finish(&self, answer: Result<(ViewId, NativeRequestOwner), String>) {
        let done = self
            .done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(done) = done {
            done(answer);
        }
    }

    fn close(self: &Arc<Self>, error: String) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.finish(Err(error));
        let opening = self.clone();
        self.cx.ui.post(Box::new(move || opening.close_created()));
    }

    fn close_created(&self) {
        if let Some(created) = self.created.get() {
            if self.cleaned.swap(true, Ordering::AcqRel) {
                return;
            }
            if let Err(error) = created.native.end_modal() {
                eprintln!("[password] cannot end the original native modal relationship: {error}");
            }
            if let Err(error) = created.native.close() {
                eprintln!("[password] cannot close the original native password window: {error}");
            }
        }
    }

    fn registered(self: &Arc<Self>, view: ViewId, origin: &DocumentContext) {
        let Some(created) = self.created.get() else {
            return;
        };
        if created.view != view {
            return;
        }
        let first = self
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|owner| {
                owner
                    .context
                    .authority_ref::<native_owner::NativeOwnerAuthority>()
                    .is_some_and(|proof| proof.origin == *origin)
            });
        if let Some(same) = first {
            if !same {
                self.close("The password renderer document was replaced".into());
            }
            return;
        }
        let owner = match native_owner::capture_owner(&self.cx.views, view, &created.generation) {
            Ok(owner)
                if owner
                    .context
                    .authority_ref::<native_owner::NativeOwnerAuthority>()
                    .is_some_and(|proof| proof.origin == *origin) =>
            {
                owner
            }
            Ok(_) => {
                return self.close(
                    "The password registration does not match its created native document".into(),
                );
            }
            Err(error) => return self.close(error),
        };
        let accepted = self.dialog_effect(&owner, &created.generation, || {
            if self.closed.load(Ordering::Acquire) {
                return false;
            }
            *self.owner.lock().unwrap_or_else(|error| error.into_inner()) = Some(owner.clone());
            true
        });
        if accepted != Some(true) {
            return self.close("The password request expired before registration".into());
        }
        let Some(loss) = owner.loss_token() else {
            return self.close("The password renderer owner lease is unavailable".into());
        };
        let weak = Arc::downgrade(self);
        let subscription = loss.on_cancel(move || {
            if let Some(opening) = weak.upgrade() {
                opening.close("The password renderer owner expired".into());
            }
        });
        self.losses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(subscription);
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.finish(Ok((view, owner)));
    }

    fn ready(self: &Arc<Self>, origin: &DocumentContext) {
        let Some(created) = self.created.get() else {
            return;
        };
        let owner = self
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(owner) = owner.filter(|owner| {
            owner
                .context
                .authority_ref::<native_owner::NativeOwnerAuthority>()
                .is_some_and(|proof| proof.origin == *origin)
        }) else {
            return;
        };
        if self.shown.swap(true, Ordering::AcqRel) {
            return;
        }
        let result = self
            .dialog_ui(&owner, &created.generation, || {
                if self.closed.load(Ordering::Acquire) {
                    return Err("The password window was closed before modal admission".to_owned());
                }
                if let Some(parent) = &created.parent {
                    created
                        .native
                        .begin_modal(parent)
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            })
            .and_then(|result| result);
        if let Err(error) = result {
            return self.close(error);
        }
        let result = self
            .dialog_ui(&owner, &created.generation, || {
                if self.closed.load(Ordering::Acquire) {
                    return Err("The password window was closed before display".to_owned());
                }
                created.native.show().map_err(|error| error.to_string())
            })
            .and_then(|result| result);
        if let Err(error) = result {
            return self.close(error);
        }
        if let Err(error) = self.focus_now() {
            self.close(error);
        }
    }

    fn focus_now(&self) -> Result<(), String> {
        let created = self
            .created
            .get()
            .ok_or("The password native identity is unavailable")?;
        let owner = self
            .owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or("The password native document has not registered")?;
        self.dialog_ui(&owner, &created.generation, || {
            if self.closed.load(Ordering::Acquire) {
                return Err("The password window was closed before focus".to_owned());
            }
            created
                .native
                .set_focus()
                .map_err(|error| error.to_string())
        })
        .and_then(|result| result)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn open_password_window_owned(
    cx: &HostContext,
    parent: Option<WindowId>,
    title: String,
    source: NativeRequestOwner,
    destination: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    done: Ready,
) -> OwnedPasswordWindowActions {
    open_password_window(
        cx,
        parent,
        title,
        Authority::Renderer {
            source,
            destination,
        },
        cancel,
        pending,
        done,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn open_password_window_prepared(
    cx: &HostContext,
    parent: Option<WindowId>,
    title: String,
    target: shell_contract::ipc::RecoveryOpenTarget,
    snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    cancel: CancelToken,
    pending: CancelToken,
    done: Ready,
) -> OwnedPasswordWindowActions {
    open_password_window(
        cx,
        parent,
        title,
        Authority::Prepared { target, snapshot },
        cancel,
        pending,
        done,
    )
}

pub(crate) fn open_password_window_saved(
    cx: &HostContext,
    parent: WindowId,
    lifecycle: Arc<crate::views::NativeWindowLifecycle>,
    title: String,
    document: shell_contract::ipc::NativeSavedDocument,
    done: Ready,
) -> Result<OwnedPasswordWindowActions, String> {
    let target = document.target();
    if !lifecycle.belongs_to(parent) {
        return Err("The saved password parent does not match its captured window".into());
    }
    let parent_window = lifecycle.window_handle()?;
    let cancel = target.cancel().clone();
    let pending = target.pending().clone();
    Ok(open_password_window(
        cx,
        Some(parent),
        title,
        Authority::Saved {
            document: Box::new(document),
            parent: Box::new(parent_window),
        },
        cancel,
        pending,
        done,
    ))
}

fn open_password_window(
    cx: &HostContext,
    parent: Option<WindowId>,
    title: String,
    authority: Authority,
    cancel: CancelToken,
    pending: CancelToken,
    done: Ready,
) -> OwnedPasswordWindowActions {
    let opening = Arc::new(Opening {
        cx: cx.clone(),
        authority,
        cancel,
        pending,
        created: OnceLock::new(),
        owner: Mutex::new(None),
        done: Mutex::new(Some(done)),
        losses: Mutex::new(Vec::new()),
        closed: AtomicBool::new(false),
        cleaned: AtomicBool::new(false),
        shown: AtomicBool::new(false),
    });
    let cleanup = opening.clone();
    let close: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || cleanup.close("The password prompt was closed".into()));
    let focused = opening.clone();
    let focus = Arc::new(move |done: Box<dyn FnOnce(Result<(), String>) + Send>| {
        let opening = focused.clone();
        focused
            .cx
            .ui
            .post(Box::new(move || done(opening.focus_now())));
    });
    let actions = OwnedPasswordWindowActions { close, focus };
    let tokens = match opening.loss_tokens() {
        Ok(tokens) => tokens,
        Err(error) => {
            opening.close(error);
            return actions;
        }
    };
    for loss in tokens {
        let weak = Arc::downgrade(&opening);
        let subscription = loss.on_cancel(move || {
            if let Some(opening) = weak.upgrade() {
                opening.close("The password request was cancelled".into());
            }
        });
        opening
            .losses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(subscription);
    }
    let ui = cx.ui.clone();
    ui.post(Box::new(move || {
        let result = opening
            .seed_ui(|| {
                if opening.closed.load(Ordering::Acquire) {
                    return Err("The password request expired before construction".to_owned());
                }
                let frames = Arc::downgrade(&opening);
                let closed = Arc::downgrade(&opening);
                let options = WindowOptions {
                    center: true,
                    minimizable: false,
                    maximizable: false,
                    fullscreenable: false,
                    on_frame: Some(Arc::new(move |view, event| {
                        let Some(opening) = frames.upgrade() else {
                            return;
                        };
                        match &event.kind {
                            NativeEventKind::DocumentRegistered { context } => {
                                opening.registered(view, context)
                            }
                            NativeEventKind::DocumentReady { context } => opening.ready(context),
                            NativeEventKind::DocumentRevoked { document, .. } => {
                                let matches = opening
                                    .owner
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .as_ref()
                                    .is_some_and(|owner| {
                                        owner
                                            .context
                                            .authority_ref::<native_owner::NativeOwnerAuthority>()
                                            .is_some_and(|proof| {
                                                proof.origin.identity() == document
                                            })
                                    });
                                if matches {
                                    opening
                                        .close("The password renderer context was revoked".into());
                                }
                            }
                            NativeEventKind::WebviewProcessFailed { .. } => {
                                opening.close("The password renderer stopped".into())
                            }
                            NativeEventKind::NavigationFailed { .. } => {
                                opening.close("The password document could not load".into())
                            }
                            _ => {}
                        }
                    })),
                    on_closed: Some(Box::new(move |_| {
                        if let Some(opening) = closed.upgrade() {
                            opening.close("The password window was closed".into());
                        }
                    })),
                    ..Default::default()
                };
                let builder_parent = if matches!(&opening.authority, Authority::Saved { .. }) {
                    None
                } else {
                    parent
                };
                let spec = WindowSpec {
                    title,
                    size: (440.0, 312.0),
                    min_size: None,
                    visible: false,
                    decorations: false,
                    transparent: true,
                    resizable: false,
                    parent: builder_parent,
                    focus: false,
                    main: false,
                    view: ViewSpec {
                        url: "about:blank".into(),
                        bridge: Bridge::Surface(Surface::ShellPdfPassword),
                    },
                };
                let (window, view, captured) =
                    if matches!(&opening.authority, Authority::Saved { .. }) {
                        let reservation = opening
                            .cx
                            .windows
                            .reserve_deferred_window(Surface::ShellPdfPassword)?;
                        let lifecycle = reservation.window_lifecycle.clone();
                        let (window, view) = opening.cx.windows.open_reserved_deferred_window(
                            spec,
                            options,
                            reservation,
                        )?;
                        (window, view, Some(lifecycle))
                    } else {
                        let (window, view) =
                            opening.cx.windows.open_deferred_window(spec, options)?;
                        (window, view, None)
                    };
                let prepared: Result<Created, &'static str> = (|| {
                    let native = if let Some(lifecycle) = &captured {
                        lifecycle
                            .window_handle()
                            .map_err(|_| "The created password native handle is unavailable")?
                    } else {
                        let label = opening
                            .cx
                            .views
                            .label_of_window(window)
                            .ok_or("The created password window is gone")?;
                        opening
                            .cx
                            .app
                            .get_window(&label)
                            .ok_or("The created password native handle is unavailable")?
                    };
                    let generation = opening
                        .cx
                        .views
                        .renderer_generation(view)
                        .ok_or("The password creation generation is unavailable")?;
                    let parent = match &opening.authority {
                        Authority::Saved { parent, .. } => Some((**parent).clone()),
                        _ => parent
                            .map(|parent| {
                                let label = opening
                                    .cx
                                    .views
                                    .label_of_window(parent)
                                    .ok_or("The password parent window is gone")?;
                                opening.cx.app.get_window(&label).ok_or(
                                    "The original password parent native handle is unavailable",
                                )
                            })
                            .transpose()?,
                    };
                    Ok(Created {
                        view,
                        generation,
                        native,
                        parent,
                    })
                })();
                match prepared {
                    Ok(created) => opening.created.set(created).map_err(|_| {
                        "The password native identity was already assigned".to_owned()
                    })?,
                    Err(error) => {
                        let _ = opening.cx.windows.close_window(window);
                        return Err(error.to_owned());
                    }
                }
                if opening.closed.load(Ordering::Acquire) {
                    opening.close_created();
                    return Err("The password request expired during construction".into());
                }
                Ok(())
            })
            .and_then(|result| result);
        if let Err(error) = result {
            return opening.close(error);
        }
        let Some(created) = opening.created.get() else {
            return opening.close("The password native identity is unavailable".into());
        };
        let result = opening
            .created_ui(&created.generation, || {
                if opening.closed.load(Ordering::Acquire) {
                    return Err("The password request expired before document load".to_owned());
                }
                opening
                    .cx
                    .windows
                    .load_view(created.view, "genoffice-app://shell/pdf-password.html")
            })
            .and_then(|result| result);
        if let Err(error) = result {
            opening.close(error);
        }
    }));
    actions
}
