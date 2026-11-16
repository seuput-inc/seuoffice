//! Captured native operations for one request-bound tab activation.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use main_core::shell::tab_manager::{TabActivationEffect, TabKind};
use shell_contract::provided::Provided;
use shell_contract::{
    ViewId,
    ipc::NativeRequestOwner,
    runtime::{Services, Views},
};
use shell_ipc_w::tabs::{
    PreparedTabUiBatch, PreparedTabUiStep, ShellWindow, TabActivationAuthority,
};
use tauri::Webview;

use super::{Hooks, NATIVE_WINDOWS};

fn captured_owner(hooks: &Hooks, view: ViewId) -> Result<NativeRequestOwner, String> {
    let generation = hooks
        .services
        .views
        .renderer_generation(view)
        .ok_or_else(|| format!("The activation view {} is unavailable.", view.0))?;
    crate::ipc::native_owner::capture_owner(&hooks.cx.views, view, &generation)
}

fn captured_view(hooks: &Hooks, view: ViewId) -> Result<Webview, String> {
    hooks
        .cx
        .windows
        .webview(view)
        .ok_or_else(|| format!("The activation view {} is gone.", view.0))
}

pub(super) fn run_accepted_ui<T>(
    ui: &dyn shell_contract::runtime::Thread,
    effects: &shell_contract::hosts::OwnedEffectsHost,
    authority: &TabActivationAuthority,
    participants: &[NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    action: impl FnOnce() -> T,
) -> Result<T, String> {
    match authority {
        TabActivationAuthority::Interactive(origin) => {
            crate::ipc::native_owner::run_accepted_ui_many(
                ui,
                origin,
                participants,
                cancel,
                pending,
                action,
            )
        }
        TabActivationAuthority::NativeInvocation { document, owner } => {
            crate::ipc::native_owner::run_accepted_native_invocation_ui_many(
                ui,
                document,
                owner,
                participants,
                cancel,
                pending,
                action,
            )
        }
        TabActivationAuthority::NativeOperation { operation, owner } => {
            crate::ipc::native_owner::run_accepted_native_operation_ui_many(
                ui,
                operation,
                owner,
                participants,
                cancel,
                pending,
                action,
            )
        }
        TabActivationAuthority::StartupRecovery {
            startup,
            shell,
            child,
            owner,
        } => crate::ipc::native_owner::run_accepted_startup_recovery_ui_many(
            ui,
            startup,
            shell,
            child,
            owner,
            participants,
            cancel,
            pending,
            action,
        ),
        TabActivationAuthority::NativeCommand {
            operation,
            expected,
            owner,
        } => {
            if !ui.is_current() {
                return Err("Native UI admission requires the UI thread.".into());
            }
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            crate::ipc::native_owner::with_native_command_preparation_effect(
                operation,
                expected,
                &captured,
                cancel,
                pending,
                || action,
            )
            .ok_or_else(|| "The captured native command is no longer available.".to_owned())
            .map(|action| action())
        }
        TabActivationAuthority::NativeCommandTarget {
            document,
            expected,
            owner,
        } => {
            if !ui.is_current() {
                return Err("Native UI admission requires the UI thread.".into());
            }
            crate::ipc::native_owner::with_native_command_target_effect(
                document.operation(),
                expected,
                document,
                owner,
                participants,
                cancel,
                pending,
                || action,
            )
            .ok_or_else(|| "The captured native command target is no longer available.".to_owned())
            .map(|action| action())
        }
        TabActivationAuthority::StartupCommand {
            startup,
            shell,
            child,
            expected,
            owner,
        } => {
            if !ui.is_current() {
                return Err("Native UI admission requires the UI thread.".into());
            }
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            crate::ipc::native_owner::with_startup_command_preparation_effect(
                startup,
                shell,
                child,
                expected,
                &captured,
                cancel,
                pending,
                || action,
            )
            .ok_or_else(|| "The captured startup command is no longer available.".to_owned())
            .map(|action| action())
        }
        TabActivationAuthority::StartupCommandTarget {
            startup,
            shell,
            document,
            expected,
            owner,
        } => {
            if !ui.is_current() {
                return Err("Native UI admission requires the UI thread.".into());
            }
            crate::ipc::native_owner::with_startup_command_target_effect(
                startup,
                shell,
                document.operation(),
                expected,
                document,
                owner,
                participants,
                cancel,
                pending,
                || action,
            )
            .ok_or_else(|| "The captured startup command target is no longer available.".to_owned())
            .map(|action| action())
        }
        TabActivationAuthority::CommandRecovery {
            command,
            child,
            owner,
        } => {
            if !ui.is_current() {
                return Err("Native UI admission requires the UI thread.".into());
            }
            crate::ipc::native_owner::with_command_recovery_target_effect(
                command,
                child,
                owner,
                participants,
                cancel,
                pending,
                || action,
            )
            .ok_or_else(|| "The command recovery document is no longer available.".to_owned())
            .map(|action| action())
        }
        TabActivationAuthority::ExistingDocumentSave {
            path,
            owner,
            additional,
        } => {
            if !ui.is_current() {
                return Err("Native UI admission requires the UI thread.".into());
            }
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            let extra: &[shell_contract::runtime::CancelToken] = additional;
            let mut action = Some(action);
            let mut accepted = None;
            path.with_effect(effects, &captured, cancel, pending, extra, &mut || {
                accepted = action.take();
                Ok(())
            })?;
            let action = accepted.ok_or("The selected native save was not admitted.")?;
            Ok(action())
        }
        TabActivationAuthority::SavedDocument {
            document,
            owner,
            additional,
        } => {
            if !ui.is_current() {
                return Err("Native UI admission requires the UI thread.".into());
            }
            let extra: Vec<_> = additional.iter().collect();
            let mut action = Some(action);
            let mut accepted = None;
            effects.with_native_saved_document_same_origin(
                document,
                owner,
                cancel,
                participants,
                cancel,
                pending,
                &extra,
                &mut |_| {
                    accepted = action.take();
                    Ok(())
                },
            )?;
            Ok((accepted.ok_or("The saved document was not admitted.")?)())
        }
        TabActivationAuthority::NativeSaveReload { receipt, owner } => {
            if !ui.is_current() {
                return Err("Native UI admission requires the UI thread.".into());
            }
            let mut action = Some(action);
            let mut accepted = None;
            effects.with_native_save_reload_many_effect(
                receipt,
                owner,
                participants,
                cancel,
                pending,
                &mut || {
                    accepted = action.take();
                    Ok(())
                },
            )?;
            Ok((accepted
                .ok_or("The reloaded document was not admitted.")?)(
            ))
        }
    }
}

fn prepared_step(
    services: Services,
    action: Box<dyn FnOnce() -> Result<(), String> + Send>,
) -> PreparedTabUiStep {
    PreparedTabUiStep::new(Box::new(
        move |origin, participants, cancel, pending, close, done| {
            if close.is_some() {
                return done(Err("A non-close UI step received a close permit".into()));
            }
            let core = services.core.clone();
            let complete = shell_ipc_w::home_files::back_to_core(core, move |result: Result<(), String>| {
                done(result.map(|()| shell_ipc_w::tabs::TabUiStepOutcome::Applied))
            });
            let Some(effects) = services
                .hosts
                .get::<shell_contract::hosts::OwnedEffectsHost>()
                .cloned()
            else {
                return complete(Err("Native activation effects are unavailable".into()));
            };
            let ui = services.ui.clone();
            let task_ui = ui.clone();
            ui.post(Box::new(move || {
                let result = run_accepted_ui(
                    task_ui.as_ref(),
                    &effects,
                    &origin,
                    &participants,
                    &cancel,
                    &pending,
                    action,
                )
                .and_then(|result| result);
                complete(result);
            }));
        },
    ))
}

fn menu_step(hooks: &Hooks, kind: TabKind, target: Option<ViewId>) -> PreparedTabUiStep {
    let services = hooks.services.clone();
    let provided: Rc<Provided> = hooks.provided.clone();
    let app = hooks.cx.app.clone();
    PreparedTabUiStep::new(Box::new(
        move |origin, participants, cancel, pending, close, done| {
            let action = match crate::host::menu::prepare_activation_menu(
                &app, kind, target, &provided, &services,
            ) {
                Ok(action) => action,
                Err(error) => return done(Err(error)),
            };
            prepared_step(services, Box::new(move || action())).run(
                origin,
                participants,
                cancel,
                pending,
                close,
                done,
            );
        },
    ))
}

struct RemoveAttempt {
    services: Services,
    effects: shell_contract::hosts::OwnedEffectsHost,
    authority: TabActivationAuthority,
    participants: Arc<[NativeRequestOwner]>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    permit: RefCell<Option<shell_contract::ipc::NativeBrowserClosePermit>>,
    action: Arc<
        dyn Fn(&mut shell_contract::ipc::NativeBrowserClosePermit) -> Result<(), String>
            + Send
            + Sync,
    >,
    running: Cell<bool>,
}

impl RemoveAttempt {
    fn run(self: &Rc<Self>, done: shell_ipc_w::tabs::TabUiStepDone) {
        if self.running.replace(true) {
            return done(Err("The captured removal is already running".into()));
        }
        let Some(mut permit) = self.permit.borrow_mut().take() else {
            self.running.set(false);
            return done(Err("The captured close permit is unavailable".into()));
        };
        let state = self.clone();
        let complete = shell_ipc_w::home_files::back_to_core(
            self.services.core.clone(),
            move |(result, permit)| {
                state.running.set(false);
                match result {
                    Ok(()) => done(Ok(
                        shell_ipc_w::tabs::TabUiStepOutcome::AppliedWithClosePermit(permit),
                    )),
                    Err(error) => {
                        *state.permit.borrow_mut() = Some(permit);
                        let retry_state = state.clone();
                        let retry: shell_ipc_w::tabs::TabUiStepRetry =
                            Rc::new(move |done| retry_state.run(done));
                        done(Ok(
                            shell_ipc_w::tabs::TabUiStepOutcome::CommittedCleanupPending {
                                error,
                                retry,
                            },
                        ));
                    }
                }
            },
        );
        let ui = self.services.ui.clone();
        let task_ui = ui.clone();
        let effects = self.effects.clone();
        let authority = self.authority.clone();
        let participants = self.participants.clone();
        let cancel = self.cancel.clone();
        let pending = self.pending.clone();
        let action = self.action.clone();
        ui.post(Box::new(move || {
            let result = permit.with_closing(|_| Ok(())).and_then(|()| {
                run_accepted_ui(
                    task_ui.as_ref(),
                    &effects,
                    &authority,
                    &participants,
                    &cancel,
                    &pending,
                    || action(&mut permit),
                )
                .and_then(|result| result)
            });
            complete((result, permit));
        }));
    }
}

fn parking_removal(
    host: Arc<super::NativeWindows>,
    webview: Webview,
    view: ViewId,
    creation: shell_contract::runtime::RendererGeneration,
    browser: shell_contract::ipc::NativeBrowserOwner,
    previous_id: shell_contract::WindowId,
    previous_lifecycle: Arc<crate::views::NativeWindowLifecycle>,
    previous: tauri::Window,
    initial_parking: Option<super::ParkingWindow>,
) -> Arc<
    dyn Fn(&mut shell_contract::ipc::NativeBrowserClosePermit) -> Result<(), String> + Send + Sync,
> {
    let selected = std::sync::Mutex::new(initial_parking);
    Arc::new(move |permit| {
        let parking = {
            let mut selected = selected
                .lock()
                .map_err(|_| "The captured parking state is unavailable")?;
            if let Some(parking) = selected.as_ref() {
                if !host
                    .parking
                    .lock()
                    .map_err(|_| "The parking state is unavailable")?
                    .as_ref()
                    .is_some_and(|current| current.same(parking))
                {
                    return Err("The captured parking window changed".into());
                }
                parking.clone()
            } else {
                let (id, window, lifecycle) = host.cx.windows.open_empty_window_handle(
                    "SeuOffice",
                    (1.0, 1.0),
                    (1.0, 1.0),
                    None,
                    false,
                )?;
                let parking = super::ParkingWindow {
                    id,
                    window,
                    lifecycle,
                };
                let mut current = host
                    .parking
                    .lock()
                    .map_err(|_| "The parking state is unavailable")?;
                if current.is_some() {
                    drop(current);
                    parking
                        .window
                        .destroy()
                        .map_err(|error| error.to_string())?;
                    host.cx
                        .views
                        .remove_window_captured(id, &parking.lifecycle)?;
                    return Err("The parking window changed before activation".into());
                }
                *current = Some(parking.clone());
                *selected = Some(parking.clone());
                parking
            }
        };
        if webview.window().label() == parking.window.label() {
            webview
                .reparent(&previous)
                .map_err(|error| format!("restoring captured parent: {error}"))?;
        }
        if webview.window().label() != previous.label() {
            return Err("The captured editor parent changed".into());
        }
        webview.hide().map_err(|error| error.to_string())?;
        if let Err(error) = webview.reparent(&parking.window) {
            return match webview.reparent(&previous) {
                Ok(()) => Err(format!("reparenting view: {error}")),
                Err(rollback) => Err(format!(
                    "reparenting view: {error}; restoring parent: {rollback}"
                )),
            };
        }
        let moved = permit.with_closing(|transition| {
            let current = host
                .parking
                .lock()
                .map_err(|_| "The parking state is unavailable")?;
            if !current
                .as_ref()
                .is_some_and(|current| current.same(&parking))
            {
                return Err("The captured parking window changed".into());
            }
            host.cx.views.move_view_between_closing(
                view,
                &creation,
                &browser,
                previous_id,
                &previous_lifecycle,
                parking.id,
                &parking.lifecycle,
                transition,
            )
        });
        if let Err(error) = moved {
            return match webview.reparent(&previous) {
                Ok(()) => Err(error),
                Err(rollback) => Err(format!("{error}; restoring parent: {rollback}")),
            };
        }
        Ok(())
    })
}
struct CloseAttempt {
    core: Arc<dyn shell_contract::runtime::Thread>,
    producer: Arc<dyn shell_ipc_w::tabs::host::NativeBrowserCloseProducer>,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: shell_contract::ipc::NativeBrowserOwner,
    generation: shell_contract::runtime::RendererGeneration,
    view: ViewId,
    permit: RefCell<Option<shell_contract::ipc::NativeBrowserClosePermit>>,
    receipt: RefCell<Option<shell_contract::ipc::NativeClosureReceipt>>,
    watching: Cell<bool>,
    dispatched: Cell<bool>,
    done: RefCell<Option<shell_ipc_w::tabs::TabUiStepDone>>,
}

impl CloseAttempt {
    fn pending(self: &Rc<Self>, error: String) {
        let done = self.done.borrow_mut().take();
        if let Some(done) = done {
            let state = self.clone();
            let retry: shell_ipc_w::tabs::TabUiStepRetry = Rc::new(move |done| state.run(done));
            done(Ok(
                shell_ipc_w::tabs::TabUiStepOutcome::CommittedCleanupPending { error, retry },
            ));
        }
    }

    fn complete(self: &Rc<Self>) -> bool {
        let receipt = self.receipt.borrow().clone();
        let Some(receipt) = receipt else {
            return false;
        };
        if !receipt.matches(&self.reservation, self.view, &self.generation)
            || receipt.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed
        {
            self.pending("The native close receipt does not match the captured browser".into());
            return true;
        }
        if let Err(error) = receipt.with_closed_owner(&self.browser, |transition| {
            if transition.matches(&self.reservation, &self.browser) {
                Ok(())
            } else {
                Err("The native close owner changed".into())
            }
        }) {
            self.pending(error);
            return true;
        }
        self.permit.borrow_mut().take();
        let done = self.done.borrow_mut().take();
        if let Some(done) = done {
            done(Ok(shell_ipc_w::tabs::TabUiStepOutcome::Applied));
        }
        true
    }

    fn run(self: &Rc<Self>, done: shell_ipc_w::tabs::TabUiStepDone) {
        if self.done.borrow().is_some() {
            return done(Err("The captured close is already running".into()));
        }
        *self.done.borrow_mut() = Some(done);
        if self.complete() {
            return;
        }
        if !self.watching.get() {
            let state = self.clone();
            let callback =
                shell_ipc_w::home_files::back_to_core(self.core.clone(), move |receipt| {
                    *state.receipt.borrow_mut() = Some(receipt);
                    state.complete();
                });
            match self.producer.watch(callback) {
                Ok(()) => self.watching.set(true),
                Err(error) => return self.pending(error),
            }
        }
        if self.dispatched.get() {
            return;
        }
        let Some(permit) = self.permit.borrow_mut().take() else {
            return self.pending("The captured close permit is unavailable".into());
        };
        match self.producer.dispatch_close(permit) {
            Ok(permit) => {
                *self.permit.borrow_mut() = Some(permit);
                self.dispatched.set(true);
            }
            Err((error, permit)) => {
                *self.permit.borrow_mut() = Some(permit);
                self.pending(error);
            }
        }
    }
}

fn close_step(
    services: Services,
    view: ViewId,
    binding: &shell_ipc_w::tabs::host::RetainedSpareBinding,
) -> Result<PreparedTabUiStep, String> {
    let generation = binding.browser.reservation.created().clone();
    if !binding.browser.reservation.matches(view, &generation) {
        return Err("The spare browser reservation does not match".into());
    }
    let producer = binding.browser.producer.clone();
    let reservation = binding.browser.reservation.clone();
    let browser = binding.browser.owner.clone();
    Ok(PreparedTabUiStep::new(Box::new(
        move |_, _, _, _, permit, done| {
            let Some(permit) = permit else {
                return done(Err("The native close permit is unavailable".into()));
            };
            let attempt = Rc::new(CloseAttempt {
                core: services.core,
                producer,
                reservation,
                browser,
                generation,
                view,
                permit: RefCell::new(Some(permit)),
                receipt: RefCell::new(None),
                watching: Cell::new(false),
                dispatched: Cell::new(false),
                done: RefCell::new(None),
            });
            attempt.run(done);
        },
    )))
}

pub(super) fn prepare(
    hooks: &Hooks,
    shell: ShellWindow,
    shell_owner: &NativeRequestOwner,
    target: Option<ViewId>,
    effects: &[TabActivationEffect<ViewId, shell_ipc_w::tabs::host::RetainedSpareBinding>],
) -> Result<PreparedTabUiBatch, String> {
    if shell_owner.view != shell.view {
        return Err("The captured shell owner belongs to another view.".into());
    }
    let generation = shell_owner
        .context
        .renderer()
        .ok_or("The captured shell renderer is unavailable.")?;
    let browser = crate::ipc::native_owner::authenticated_browser_owner(&shell_owner.context)
        .ok_or("The captured shell browser owner is unavailable")?;
    let shell_capture = hooks.cx.views.capture_closing_view(
        shell.view,
        captured_view(hooks, shell.view)?,
        browser,
    )?;
    let (window, _) = hooks
        .cx
        .views
        .view_window_captured(shell.view, generation)?;
    if window != shell.window || shell_capture.parent != shell.window {
        return Err("The captured shell view is not in the activation window.".into());
    }
    let shell_view = shell_capture.webview;
    let mut participants = Vec::new();
    for effect in effects {
        if let Some(view) = effect.view().copied() {
            if effects.iter().any(|candidate| matches!(candidate,
                TabActivationEffect::RemoveChild(closing, _) | TabActivationEffect::Close(closing, _) if *closing == view)) {
                continue;
            }
            if view != shell.view
                && !participants
                    .iter()
                    .any(|owner: &NativeRequestOwner| owner.view == view)
            {
                participants.push(captured_owner(hooks, view)?);
            }
        }
    }
    let host = NATIVE_WINDOWS
        .get()
        .cloned()
        .ok_or("The native tab window host is unavailable.")?;
    let mut steps = Vec::with_capacity(effects.len());
    for effect in effects {
        let step = match effect {
            TabActivationEffect::Visible(view, visible) => {
                let webview = captured_view(hooks, *view)?;
                let visible = *visible;
                Some(prepared_step(
                    hooks.services.clone(),
                    Box::new(move || {
                        if visible {
                            webview.show()
                        } else {
                            webview.hide()
                        }
                        .map_err(|error| error.to_string())
                    }),
                ))
            }
            TabActivationEffect::Bounds(view, bounds) => {
                let webview = captured_view(hooks, *view)?;
                let bounds = *bounds;
                Some(prepared_step(
                    hooks.services.clone(),
                    Box::new(move || {
                        webview
                            .set_position(tauri::LogicalPosition::new(bounds.x, bounds.y))
                            .map_err(|error| error.to_string())?;
                        webview
                            .set_size(tauri::LogicalSize::new(bounds.width, bounds.height))
                            .map_err(|error| error.to_string())
                    }),
                ))
            }
            TabActivationEffect::Focus(view) => {
                let webview = captured_view(hooks, *view)?;
                Some(prepared_step(
                    hooks.services.clone(),
                    Box::new(move || webview.set_focus().map_err(|error| error.to_string())),
                ))
            }
            TabActivationEffect::FocusShell => {
                let webview = shell_view.clone();
                Some(prepared_step(
                    hooks.services.clone(),
                    Box::new(move || webview.set_focus().map_err(|error| error.to_string())),
                ))
            }
            TabActivationEffect::Menu(kind) => Some(menu_step(hooks, *kind, target)),
            TabActivationEffect::RemoveChild(view, binding) => {
                let view = *view;
                let candidate = captured_view(hooks, view)?;
                let captured =
                    host.cx
                        .views
                        .capture_closing_view(view, candidate, &binding.browser.owner)?;
                if !captured
                    .proof
                    .reservation()
                    .same_reservation(&binding.browser.reservation)
                    || !captured
                        .creation
                        .same_generation(binding.browser.reservation.created())
                {
                    return Err("The captured spare browser reservation changed".into());
                }
                let previous = captured.parent_lifecycle.window_handle()?;
                let parking = host
                    .parking
                    .lock()
                    .map_err(|_| "The parking state is unavailable")?
                    .clone();
                let action = parking_removal(
                    host.clone(),
                    captured.webview,
                    view,
                    captured.creation,
                    captured.owner,
                    captured.parent,
                    captured.parent_lifecycle,
                    previous,
                    parking,
                );
                let services = hooks.services.clone();
                let effects = services
                    .hosts
                    .get::<shell_contract::hosts::OwnedEffectsHost>()
                    .cloned()
                    .ok_or("The native owned effect host is unavailable")?;
                let reservation = binding.browser.reservation.clone();
                Some(PreparedTabUiStep::new(Box::new(
                    move |authority, participants, cancel, pending, permit, done| {
                        let Some(permit) = permit else {
                            return done(Err("The native close permit is unavailable".into()));
                        };
                        let action = action.clone();
                        let guarded = Arc::new(
                            move |permit: &mut shell_contract::ipc::NativeBrowserClosePermit| {
                                if !permit.reservation().same_reservation(&reservation) {
                                    return Err(
                                        "The close permit belongs to another browser".into()
                                    );
                                }
                                action(permit)
                            },
                        );
                        let attempt = Rc::new(RemoveAttempt {
                            services,
                            effects,
                            authority,
                            participants,
                            cancel,
                            pending,
                            permit: RefCell::new(Some(permit)),
                            action: guarded,
                            running: Cell::new(false),
                        });
                        attempt.run(done);
                    },
                )))
            }
            TabActivationEffect::Close(view, binding) => {
                Some(close_step(hooks.services.clone(), *view, binding)?)
            }
            TabActivationEffect::SetSpareTimer(_)
            | TabActivationEffect::ClearSpareTimer
            | TabActivationEffect::ActiveDocs(_)
            | TabActivationEffect::ActiveSheets(_)
            | TabActivationEffect::ActiveSlides(_)
            | TabActivationEffect::Changed
            | TabActivationEffect::SplitChanged(_) => None,
        };
        steps.push(step);
    }
    Ok(PreparedTabUiBatch {
        shell: shell_owner.clone(),
        participants,
        steps,
    })
}
