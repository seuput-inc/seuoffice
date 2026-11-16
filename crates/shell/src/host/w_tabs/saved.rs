use super::Hooks;
use main_core::shell::tab_manager::TabKind;
use shell_contract::{
    hosts::{OwnedEffectsHost, SavedSessionReceipt},
    ipc::NativeRequestOwner,
    runtime::CancelToken,
};
use shell_ipc_w::tabs::{SavedPublication, SavedTabAck, TabsState};

fn captured_title_target<T>(
    captured: Option<(shell_ipc_w::tabs::window_snapshot::WindowReservation, T)>,
    requested: &shell_ipc_w::tabs::window_snapshot::WindowReservation,
) -> Result<T, String> {
    let (reservation, target) = captured.ok_or("saved detached window is unavailable")?;
    if !reservation.same_reservation(requested) {
        return Err("saved detached window was replaced".into());
    }
    Ok(target)
}

pub(super) fn prepare(
    hooks: &Hooks,
    origin: NativeRequestOwner,
    destination: NativeRequestOwner,
    mut participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
) -> Result<SavedPublication, String> {
    participants.push(destination.clone());
    let extra: Vec<_> = additional.iter().collect();
    effects.with_owned_effect_many_extra(
        &origin,
        &participants,
        &cancel,
        &pending,
        &extra,
        &mut || Ok(()),
    )?;
    let saved_view = destination.view;
    let tabs = hooks
        .provided
        .all::<TabsState>()
        .into_iter()
        .next()
        .ok_or("tab services unavailable")?;
    let (strip, detached) = tabs.session_snapshot()?;
    let mut rows = Vec::new();
    for tab in strip
        .get("tabs")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .chain(detached.iter())
    {
        if let Some(id) = tab.get("id").and_then(serde_json::Value::as_str)
            && let Some(view) = tabs.tab_contents(id)
        {
            rows.push((id.to_owned(), view));
        }
    }
    let session = crate::host::recovery_launch::capture_saved_session(
        &strip,
        &detached,
        &rows,
        destination.view,
    )?;
    let menu = crate::host::menu::SavedMenu::capture_many_extra(
        &origin,
        &destination,
        &participants,
        &cancel,
        &pending,
        &additional,
        &effects,
    )?;
    let hwp = hooks
        .provided
        .all::<shell_ipc_d2::hwp::HwpEditors>()
        .into_iter()
        .next();
    let mut hwp_targets = Vec::new();
    for (id, view) in &rows {
        if tabs
            .details_of(*view)
            .is_some_and(|details| details.kind == TabKind::Hwp)
        {
            let target = (|| {
                let generation = hooks
                    .services
                    .views
                    .renderer_generation(*view)
                    .ok_or("HWP renderer unavailable")?;
                let native = effects.capture_owner(*view, &generation)?;
                shell_contract::editors::editor_for(&hooks.provided, *view)
                    .ok_or("HWP editor unavailable")?
                    .capture_document_owner(*view, native)
                    .map_err(|error| error.to_string())
            })();
            hwp_targets.push((id.clone(), *view, target));
        }
    }
    let native_window = hooks
        .services
        .views
        .sender_of(destination.view)
        .and_then(|sender| {
            hooks
                .cx
                .views
                .window_lifecycle(sender.window)
                .map(|lifecycle| {
                    let reservation = shell_ipc_w::tabs::window_snapshot::WindowReservation::new(
                        sender.window,
                        lifecycle.clone(),
                    );
                    (reservation, lifecycle)
                })
        });
    let core = hooks.services.core.clone();
    let session_menu = menu.clone();
    let session_origin = origin.clone();
    let session_participants = participants.clone();
    let session_cancel = cancel.clone();
    let session_pending = pending.clone();
    let session_additional = additional.clone();
    let session_effects = effects.clone();
    let publish = std::rc::Rc::new(
        move |ack: SavedTabAck, done: Box<dyn FnOnce(Result<(), String>)>| {
            let extra: Vec<_> = additional.iter().collect();
            let accepted = effects.with_owned_effect_many_extra(
                &origin,
                &participants,
                &cancel,
                &pending,
                &extra,
                &mut || Ok(()),
            );
            if let Err(error) = accepted {
                done(Err(error));
                return;
            }
            if let Some(args) = ack.event_args()
                && let Some(menu) = &menu
            {
                if let Err(error) = menu.emit("tabs:changed", args) {
                    done(Err(error));
                    return;
                }
            }
            if let Some(changed) = &ack.changed {
                if let Some(shell) = ack.shell {
                    let ids: Vec<_> = changed.tabs.iter().map(|tab| tab.id.clone()).collect();
                    if let Err(error) = effects.with_owned_effect_many_extra(
                        &origin,
                        &participants,
                        &cancel,
                        &pending,
                        &extra,
                        &mut || {
                            crate::host::recovery_openers::retain_bound(shell.window, &ids);
                            Ok(())
                        },
                    ) {
                        done(Err(error));
                        return;
                    }
                }
                for (id, view, target) in &hwp_targets {
                    let Some(tab) = changed.tabs.iter().find(|tab| &tab.id == id) else {
                        continue;
                    };
                    let target = match target {
                        Ok(target) => target,
                        Err(error) => {
                            done(Err(error.clone()));
                            return;
                        }
                    };
                    let Some(hwp) = &hwp else {
                        done(Err("HWP recovery service unavailable".into()));
                        return;
                    };
                    let mut targets = participants.clone();
                    targets.push(target.clone());
                    if let Err(error) = effects.with_owned_effect_many_extra(
                        &origin,
                        &targets,
                        &cancel,
                        &pending,
                        &extra,
                        &mut || {
                            hwp.set_recovery_title(*view, &tab.title);
                            Ok(())
                        },
                    ) {
                        done(Err(error));
                        return;
                    }
                }
            }
            let menu = menu.clone();
            let refresh = move |result: Result<(), String>| match result {
                Err(error) => done(Err(error)),
                Ok(()) => crate::host::menu::doc_links::refresh_saved_menu(menu, done),
            };
            match ack.detached_title {
                Some((reservation, title)) => {
                    let lifecycle = match captured_title_target(native_window.clone(), &reservation)
                    {
                        Ok(lifecycle) => lifecycle,
                        Err(error) => return refresh(Err(error)),
                    };
                    let done = shell_ipc_w::home_files::back_to_core(core.clone(), refresh);
                    effects.post_ui_owned_many_extra(
                        origin.clone(),
                        participants.clone(),
                        cancel.clone(),
                        pending.clone(),
                        additional.clone(),
                        Box::new(move || lifecycle.set_title(&title)),
                        done,
                    );
                }
                None => refresh(Ok(())),
            }
        },
    );
    let session = Box::new(move |ack: SavedTabAck| {
        let Some(id) = ack
            .session_views
            .iter()
            .find(|(_, view)| *view == saved_view)
            .map(|(id, _)| id)
        else {
            return Err("saved session lost its captured document".into());
        };
        if !rows.iter().any(|(captured, _)| captured == id) {
            return Err("saved session tab identity changed".into());
        }
        if let Some(path) = &ack.saved_path {
            let row = ack
                .session_strip
                .get("tabs")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .chain(ack.session_detached.iter())
                .find(|row| row.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str()));
            if row
                .and_then(|row| row.get("filePath"))
                .and_then(serde_json::Value::as_str)
                != Some(path.as_str())
            {
                return Err("saved session final path does not match its committed tab".into());
            }
        }
        Ok(SavedSessionReceipt::Pending(Box::new(move |done| {
            if let Some(menu) = &session_menu
                && let Err(error) = menu.accept_completed_snapshot()
            {
                done(Err(error));
                return;
            }
            crate::host::recovery_launch::note_live_session_snapshot_owned(
                session,
                ack.session_updates,
                session_origin,
                session_participants,
                session_cancel,
                session_pending,
                session_additional,
                session_effects,
                done,
            );
        })))
    });
    Ok(SavedPublication { publish, session })
}

pub(super) fn prepare_recovery(
    hooks: &Hooks,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    receiver: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    effects: OwnedEffectsHost,
) -> Result<SavedPublication, String> {
    effects.with_recovery_open_effect_pending(
        &receipt,
        &receiver,
        &cancel,
        &pending,
        &mut || Ok(()),
    )?;
    let saved_view = receiver.view;
    let tabs = hooks
        .provided
        .all::<TabsState>()
        .into_iter()
        .next()
        .ok_or("tab services unavailable")?;
    let (strip, detached) = tabs.session_snapshot()?;
    let mut rows = Vec::new();
    for tab in strip
        .get("tabs")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .chain(detached.iter())
    {
        if let Some(id) = tab.get("id").and_then(serde_json::Value::as_str)
            && let Some(view) = tabs.tab_contents(id)
        {
            rows.push((id.to_owned(), view));
        }
    }
    let present = rows.iter().any(|(_, view)| *view == receiver.view);
    let session = if present {
        crate::host::recovery_launch::capture_saved_session(
            &strip,
            &detached,
            &rows,
            receiver.view,
        )?
    } else {
        crate::host::recovery_launch::capture_activation_session(&strip, &detached, &rows)?
    };
    let menu = crate::host::menu::RecoverySavedMenu::capture(
        &receipt, &receiver, &cancel, &pending, &effects,
    )?;
    let hwp = hooks
        .provided
        .all::<shell_ipc_d2::hwp::HwpEditors>()
        .into_iter()
        .next();
    let mut hwp_targets = Vec::new();
    for (id, view) in &rows {
        if tabs
            .details_of(*view)
            .is_some_and(|details| details.kind == TabKind::Hwp)
        {
            let target = (|| {
                let generation = hooks
                    .services
                    .views
                    .renderer_generation(*view)
                    .ok_or("HWP renderer unavailable")?;
                let native = effects.capture_owner(*view, &generation)?;
                shell_contract::editors::editor_for(&hooks.provided, *view)
                    .ok_or("HWP editor unavailable")?
                    .capture_document_owner(*view, native)
                    .map_err(|error| error.to_string())
            })();
            hwp_targets.push((id.clone(), *view, target));
        }
    }
    let native_window = hooks
        .services
        .views
        .sender_of(receiver.view)
        .and_then(|sender| {
            hooks
                .cx
                .views
                .window_lifecycle(sender.window)
                .map(|lifecycle| {
                    let reservation = shell_ipc_w::tabs::window_snapshot::WindowReservation::new(
                        sender.window,
                        lifecycle.clone(),
                    );
                    (reservation, lifecycle)
                })
        });
    let core = hooks.services.core.clone();
    let ui = hooks.services.ui.clone();
    let session_receipt = receipt.clone();
    let session_receiver = receiver.clone();
    let session_cancel = cancel.clone();
    let session_pending = pending.clone();
    let session_effects = effects.clone();
    let publish = std::rc::Rc::new(
        move |ack: SavedTabAck, done: Box<dyn FnOnce(Result<(), String>)>| {
            if let Err(error) = effects.with_recovery_open_effect_pending(
                &receipt,
                &receiver,
                &cancel,
                &pending,
                &mut || Ok(()),
            ) {
                return done(Err(error));
            }
            if let Some(args) = ack.event_args()
                && let Some(menu) = &menu
            {
                if let Err(error) = menu.emit("tabs:changed", args) {
                    return done(Err(error));
                }
            }
            if let Some(changed) = &ack.changed {
                if let Some(shell) = ack.shell {
                    let ids: Vec<_> = changed.tabs.iter().map(|tab| tab.id.clone()).collect();
                    if let Err(error) = effects.with_recovery_open_effect_pending(
                        &receipt,
                        &receiver,
                        &cancel,
                        &pending,
                        &mut || {
                            crate::host::recovery_openers::retain_bound(shell.window, &ids);
                            Ok(())
                        },
                    ) {
                        return done(Err(error));
                    }
                }
                for (id, view, target) in &hwp_targets {
                    let Some(tab) = changed.tabs.iter().find(|tab| &tab.id == id) else {
                        continue;
                    };
                    let target = match target {
                        Ok(target) => target,
                        Err(error) => return done(Err(error.clone())),
                    };
                    let Some(hwp) = &hwp else {
                        return done(Err("HWP recovery service unavailable".into()));
                    };
                    if let Err(error) = effects.with_recovery_open_effect_many(
                        &receipt,
                        &receiver,
                        &[target.clone()],
                        &cancel,
                        &pending,
                        &mut || {
                            hwp.set_recovery_title(*view, &tab.title);
                            Ok(())
                        },
                    ) {
                        return done(Err(error));
                    }
                }
            }
            let menu = menu.clone();
            let refresh = move |result: Result<(), String>| match result {
                Err(error) => done(Err(error)),
                Ok(()) => crate::host::menu::doc_links::refresh_recovery_saved_menu(menu, done),
            };
            match ack.detached_title {
                Some((reservation, title)) => {
                    let lifecycle = match captured_title_target(native_window.clone(), &reservation)
                    {
                        Ok(lifecycle) => lifecycle,
                        Err(error) => return refresh(Err(error)),
                    };
                    let reply = shell_ipc_w::home_files::back_to_core(core.clone(), refresh);
                    let receipt = receipt.clone();
                    let receiver = receiver.clone();
                    let cancel = cancel.clone();
                    let pending = pending.clone();
                    let effects = effects.clone();
                    ui.post(Box::new(move || {
                        let mut action = Some(move || lifecycle.set_title(&title));
                        let mut accepted = None;
                        let result = effects
                            .with_recovery_open_effect_pending(
                                &receipt,
                                &receiver,
                                &cancel,
                                &pending,
                                &mut || {
                                    accepted = action.take();
                                    Ok(())
                                },
                            )
                            .and_then(|()| {
                                accepted.ok_or("the detached title action was not admitted")?()
                            });
                        reply(result);
                    }));
                }
                None => refresh(Ok(())),
            }
        },
    );
    let session = Box::new(move |ack: SavedTabAck| {
        if present {
            let Some(id) = ack
                .session_views
                .iter()
                .find(|(_, view)| *view == saved_view)
                .map(|(id, _)| id)
            else {
                return Err("saved session lost its captured document".into());
            };
            if !rows.iter().any(|(captured, _)| captured == id) {
                return Err("saved session tab identity changed".into());
            }
            if let Some(path) = &ack.saved_path {
                let row = ack
                    .session_strip
                    .get("tabs")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .chain(ack.session_detached.iter())
                    .find(|row| {
                        row.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str())
                    });
                if row
                    .and_then(|row| row.get("filePath"))
                    .and_then(serde_json::Value::as_str)
                    != Some(path.as_str())
                {
                    return Err("saved session final path does not match its committed tab".into());
                }
            }
        } else if ack
            .session_views
            .iter()
            .any(|(_, view)| *view == saved_view)
        {
            return Err("saved session unexpectedly acquired the recovery document".into());
        }
        Ok(SavedSessionReceipt::Pending(Box::new(move |done| {
            crate::host::recovery_launch::note_live_session_snapshot_recovery(
                session,
                ack.session_updates,
                session_receipt,
                session_receiver,
                session_cancel,
                session_pending,
                session_effects,
                done,
            );
        })))
    });
    Ok(SavedPublication { publish, session })
}

pub(super) fn prepare_existing_document(
    hooks: &Hooks,
    destination: shell_contract::ipc::NativeSaveDestination,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
) -> Result<SavedPublication, String> {
    let extra: Vec<_> = additional.iter().collect();
    effects.with_existing_document_destination_effect(
        &destination,
        destination.path(),
        &participants,
        &cancel,
        &pending,
        &extra,
        &mut || Ok(()),
    )?;
    let binding = destination.binding().clone();
    let saved_view = binding.owner().view;
    let tabs = hooks
        .provided
        .all::<TabsState>()
        .into_iter()
        .next()
        .ok_or("tab services unavailable")?;
    let (strip, detached) = tabs.session_snapshot()?;
    let mut rows = Vec::new();
    for tab in strip
        .get("tabs")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .chain(detached.iter())
    {
        if let Some(id) = tab.get("id").and_then(serde_json::Value::as_str)
            && let Some(view) = tabs.tab_contents(id)
        {
            rows.push((id.to_owned(), view));
        }
    }
    let session =
        crate::host::recovery_launch::capture_saved_session(&strip, &detached, &rows, saved_view)?;
    let menu = crate::host::menu::ExistingDocumentSavedMenu::capture(
        &binding,
        &participants,
        &cancel,
        &pending,
        &additional,
        &effects,
    )?;
    let hwp = hooks
        .provided
        .all::<shell_ipc_d2::hwp::HwpEditors>()
        .into_iter()
        .next();
    let mut hwp_targets = Vec::new();
    for (id, view) in &rows {
        if tabs
            .details_of(*view)
            .is_some_and(|details| details.kind == TabKind::Hwp)
        {
            let target = (|| {
                let generation = hooks
                    .services
                    .views
                    .renderer_generation(*view)
                    .ok_or("HWP renderer unavailable")?;
                let native = effects.capture_owner(*view, &generation)?;
                shell_contract::editors::editor_for(&hooks.provided, *view)
                    .ok_or("HWP editor unavailable")?
                    .capture_document_owner(*view, native)
                    .map_err(|error| error.to_string())
            })();
            hwp_targets.push((id.clone(), *view, target));
        }
    }
    let native_window = hooks
        .services
        .views
        .sender_of(saved_view)
        .and_then(|sender| {
            hooks
                .cx
                .views
                .window_lifecycle(sender.window)
                .map(|lifecycle| {
                    let reservation = shell_ipc_w::tabs::window_snapshot::WindowReservation::new(
                        sender.window,
                        lifecycle.clone(),
                    );
                    (reservation, lifecycle)
                })
        });
    let core = hooks.services.core.clone();
    let session_destination = destination.clone();
    let session_participants = participants.clone();
    let session_cancel = cancel.clone();
    let session_pending = pending.clone();
    let session_additional = additional.clone();
    let session_effects = effects.clone();
    let publish = std::rc::Rc::new(
        move |ack: SavedTabAck, done: Box<dyn FnOnce(Result<(), String>)>| {
            let extra: Vec<_> = additional.iter().collect();
            if let Err(error) = effects.with_existing_document_destination_effect(
                &destination,
                destination.path(),
                &participants,
                &cancel,
                &pending,
                &extra,
                &mut || Ok(()),
            ) {
                return done(Err(error));
            }
            if let Some(args) = ack.event_args()
                && let Some(menu) = &menu
            {
                if let Err(error) = menu.emit("tabs:changed", args) {
                    return done(Err(error));
                }
            }
            if let Some(changed) = &ack.changed {
                if let Some(shell) = ack.shell {
                    let ids: Vec<_> = changed.tabs.iter().map(|tab| tab.id.clone()).collect();
                    if let Err(error) = effects.with_existing_document_destination_effect(
                        &destination,
                        destination.path(),
                        &participants,
                        &cancel,
                        &pending,
                        &extra,
                        &mut || {
                            crate::host::recovery_openers::retain_bound(shell.window, &ids);
                            Ok(())
                        },
                    ) {
                        return done(Err(error));
                    }
                }
                for (id, view, target) in &hwp_targets {
                    let Some(tab) = changed.tabs.iter().find(|tab| &tab.id == id) else {
                        continue;
                    };
                    let target = match target {
                        Ok(target) => target,
                        Err(error) => return done(Err(error.clone())),
                    };
                    let Some(hwp) = &hwp else {
                        return done(Err("HWP recovery service unavailable".into()));
                    };
                    let mut targets = participants.clone();
                    targets.push(target.clone());
                    if let Err(error) = effects.with_existing_document_destination_effect(
                        &destination,
                        destination.path(),
                        &targets,
                        &cancel,
                        &pending,
                        &extra,
                        &mut || {
                            hwp.set_recovery_title(*view, &tab.title);
                            Ok(())
                        },
                    ) {
                        return done(Err(error));
                    }
                }
            }
            let menu = menu.clone();
            let refresh = move |result: Result<(), String>| match result {
                Err(error) => done(Err(error)),
                Ok(()) => {
                    crate::host::menu::doc_links::refresh_existing_document_saved_menu(menu, done)
                }
            };
            match ack.detached_title {
                Some((reservation, title)) => {
                    let lifecycle = match captured_title_target(native_window.clone(), &reservation)
                    {
                        Ok(lifecycle) => lifecycle,
                        Err(error) => return refresh(Err(error)),
                    };
                    let done = shell_ipc_w::home_files::back_to_core(core.clone(), refresh);
                    effects.post_ui_existing_document_save_extra(
                        binding.clone(),
                        participants.clone(),
                        cancel.clone(),
                        pending.clone(),
                        additional.clone(),
                        Box::new(move || lifecycle.set_title(&title)),
                        done,
                    );
                }
                None => refresh(Ok(())),
            }
        },
    );
    let session = Box::new(move |ack: SavedTabAck| {
        let Some(id) = ack
            .session_views
            .iter()
            .find(|(_, view)| *view == saved_view)
            .map(|(id, _)| id)
        else {
            return Err("saved session lost its captured document".into());
        };
        if !rows.iter().any(|(captured, _)| captured == id) {
            return Err("saved session tab identity changed".into());
        }
        if let Some(path) = &ack.saved_path {
            let row = ack
                .session_strip
                .get("tabs")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .chain(ack.session_detached.iter())
                .find(|row| row.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str()));
            if row
                .and_then(|row| row.get("filePath"))
                .and_then(serde_json::Value::as_str)
                != Some(path.as_str())
            {
                return Err("saved session final path does not match its committed tab".into());
            }
        }
        Ok(SavedSessionReceipt::Pending(Box::new(move |done| {
            crate::host::recovery_launch::note_live_session_snapshot_existing_document(
                session,
                ack.session_updates,
                session_destination,
                session_participants,
                session_cancel,
                session_pending,
                session_additional,
                session_effects,
                done,
            );
        })))
    });
    Ok(SavedPublication { publish, session })
}

pub(super) fn prepare_saved_document(
    hooks: &Hooks,
    document: shell_contract::ipc::NativeSavedDocument,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
) -> Result<SavedPublication, String> {
    let extra: Vec<_> = additional.iter().collect();
    effects.with_native_saved_document_input(
        &document,
        &participants,
        &cancel,
        &pending,
        &extra,
        &mut |_| Ok(()),
    )?;
    let saved_view = document.owner().view;
    let tabs = hooks
        .provided
        .all::<TabsState>()
        .into_iter()
        .next()
        .ok_or("tab services unavailable")?;
    let (strip, detached) = tabs.session_snapshot()?;
    let mut rows = Vec::new();
    for tab in strip
        .get("tabs")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .chain(detached.iter())
    {
        if let Some(id) = tab.get("id").and_then(serde_json::Value::as_str)
            && let Some(view) = tabs.tab_contents(id)
        {
            rows.push((id.to_owned(), view));
        }
    }
    let session =
        crate::host::recovery_launch::capture_saved_session(&strip, &detached, &rows, saved_view)?;
    let menu = crate::host::menu::SavedDocumentMenu::capture(
        &document,
        &participants,
        &cancel,
        &pending,
        &additional,
        &effects,
    )?;
    let hwp = hooks
        .provided
        .all::<shell_ipc_d2::hwp::HwpEditors>()
        .into_iter()
        .next();
    let mut hwp_targets = Vec::new();
    for (id, view) in &rows {
        if tabs
            .details_of(*view)
            .is_some_and(|details| details.kind == TabKind::Hwp)
        {
            let target = (|| {
                let generation = hooks
                    .services
                    .views
                    .renderer_generation(*view)
                    .ok_or("HWP renderer unavailable")?;
                let native = effects.capture_owner(*view, &generation)?;
                shell_contract::editors::editor_for(&hooks.provided, *view)
                    .ok_or("HWP editor unavailable")?
                    .capture_document_owner(*view, native)
                    .map_err(|error| error.to_string())
            })();
            hwp_targets.push((id.clone(), *view, target));
        }
    }
    let native_window = hooks
        .cx
        .views
        .view_window_captured(saved_view, document.target().created())
        .ok()
        .map(|(window, lifecycle)| {
            (
                shell_ipc_w::tabs::window_snapshot::WindowReservation::new(
                    window,
                    lifecycle.clone(),
                ),
                lifecycle,
            )
        });
    let core = hooks.services.core.clone();
    let session_document = document.clone();
    let session_participants = participants.clone();
    let session_cancel = cancel.clone();
    let session_pending = pending.clone();
    let session_additional = additional.clone();
    let session_effects = effects.clone();
    let publish = std::rc::Rc::new(
        move |ack: SavedTabAck, done: Box<dyn FnOnce(Result<(), String>)>| {
            let extra: Vec<_> = additional.iter().collect();
            if let Err(error) = effects.with_native_saved_document_input(
                &document,
                &participants,
                &cancel,
                &pending,
                &extra,
                &mut |_| Ok(()),
            ) {
                return done(Err(error));
            }
            if let Some(args) = ack.event_args()
                && let Some(menu) = &menu
            {
                if let Err(error) = menu.emit("tabs:changed", args) {
                    return done(Err(error));
                }
            }
            if let Some(changed) = &ack.changed {
                if let Some(shell) = ack.shell {
                    let ids: Vec<_> = changed.tabs.iter().map(|tab| tab.id.clone()).collect();
                    if let Err(error) = effects.with_native_saved_document_input(
                        &document,
                        &participants,
                        &cancel,
                        &pending,
                        &extra,
                        &mut |_| {
                            crate::host::recovery_openers::retain_bound(shell.window, &ids);
                            Ok(())
                        },
                    ) {
                        return done(Err(error));
                    }
                }
                for (id, view, target) in &hwp_targets {
                    let Some(tab) = changed.tabs.iter().find(|tab| &tab.id == id) else {
                        continue;
                    };
                    let target = match target {
                        Ok(target) => target,
                        Err(error) => return done(Err(error.clone())),
                    };
                    let Some(hwp) = &hwp else {
                        return done(Err("HWP recovery service unavailable".into()));
                    };
                    let mut targets = participants.clone();
                    targets.push(target.clone());
                    if let Err(error) = effects.with_native_saved_document_input(
                        &document,
                        &targets,
                        &cancel,
                        &pending,
                        &extra,
                        &mut |_| {
                            hwp.set_recovery_title(*view, &tab.title);
                            Ok(())
                        },
                    ) {
                        return done(Err(error));
                    }
                }
            }
            let menu = menu.clone();
            let refresh = move |result: Result<(), String>| match result {
                Err(error) => done(Err(error)),
                Ok(()) => crate::host::menu::doc_links::refresh_saved_document_menu(menu, done),
            };
            match ack.detached_title {
                Some((reservation, title)) => {
                    let lifecycle = match captured_title_target(native_window.clone(), &reservation)
                    {
                        Ok(lifecycle) => lifecycle,
                        Err(error) => return refresh(Err(error)),
                    };
                    let done = shell_ipc_w::home_files::back_to_core(core.clone(), refresh);
                    effects.post_ui_native_saved_document_input(
                        document.clone(),
                        participants.clone(),
                        cancel.clone(),
                        pending.clone(),
                        additional.clone(),
                        Box::new(move || lifecycle.set_title(&title)),
                        done,
                    );
                }
                None => refresh(Ok(())),
            }
        },
    );
    let session = Box::new(move |ack: SavedTabAck| {
        let Some(id) = ack
            .session_views
            .iter()
            .find(|(_, view)| *view == saved_view)
            .map(|(id, _)| id)
        else {
            return Err("saved session lost its captured document".into());
        };
        if !rows.iter().any(|(captured, _)| captured == id) {
            return Err("saved session tab identity changed".into());
        }
        if let Some(path) = &ack.saved_path {
            if std::path::Path::new(path) != session_document.target().path().path() {
                return Err("saved session path does not match its retained input".into());
            }
            let row = ack
                .session_strip
                .get("tabs")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .chain(ack.session_detached.iter())
                .find(|row| row.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str()));
            if row
                .and_then(|row| row.get("filePath"))
                .and_then(serde_json::Value::as_str)
                != Some(path.as_str())
            {
                return Err("saved session final path does not match its committed tab".into());
            }
        }
        Ok(SavedSessionReceipt::Pending(Box::new(move |done| {
            crate::host::recovery_launch::note_live_session_snapshot_saved_document(
                session,
                ack.session_updates,
                session_document,
                session_participants,
                session_cancel,
                session_pending,
                session_additional,
                session_effects,
                done,
            );
        })))
    });
    Ok(SavedPublication { publish, session })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_ipc_w::tabs::window_snapshot::{
        NativeWindowGuard, NativeWindowProof, WindowObservation, WindowReservation,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Guard;
    impl NativeWindowGuard for Guard {
        fn revision(&self) -> u64 {
            0
        }
        fn stable(&self) -> bool {
            true
        }
        fn destroyed(&self) -> bool {
            false
        }
    }
    struct Proof(Arc<AtomicUsize>);
    impl NativeWindowProof for Proof {
        fn lock(&self) -> Box<dyn NativeWindowGuard + '_> {
            Box::new(Guard)
        }
        fn observe(
            self: Arc<Self>,
            done: Box<dyn FnOnce(Result<WindowObservation, String>) + Send>,
        ) {
            done(Err("No native window exists in this fixture".into()));
        }
        fn set_title(&self, _: &str) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn detached_saved_title_never_targets_replacement_window() {
        let writes = Arc::new(AtomicUsize::new(0));
        let original =
            WindowReservation::new(shell_contract::WindowId(4), Arc::new(Proof(writes.clone())));
        let replacement = WindowReservation::new(
            shell_contract::WindowId(4),
            Arc::new(Proof(Arc::new(AtomicUsize::new(0)))),
        );
        let captured = Some((original.clone(), original.clone()));
        assert!(captured_title_target(captured.clone(), &replacement).is_err());
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        captured_title_target(captured, &original)
            .unwrap()
            .set_title("saved.docx")
            .unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }
}
