use super::*;
use shell_contract::WindowId;

pub(in crate::diagram) struct PendingClose {
    scope: OwnedSave,
    then: Then<bool>,
    _subscriptions: Vec<CancelSubscription>,
}

impl PendingClose {
    pub(in crate::diagram) fn cancel_and_finish(self) {
        self.scope.pending.cancel();
        (self.then)(Err(thrown("The diagram view was closed")));
    }
}

impl DiagramViews {
    fn finish_owned_close(&self, view: ViewId, id: &str, result: Result<bool, IpcError>) {
        let close = self.with_state(|state| state.views.get_mut(&view)?.owned_closes.remove(id));
        if let Some(close) = close {
            let result = close.scope.effect(|| result);
            (close.then)(result);
            close.scope.pending.cancel();
        }
    }

    pub(in crate::diagram) fn request_owned_close(&self, view: ViewId, parent: Option<WindowId>,
        origin: NativeRequestOwner, cancel: CancelToken, then: Then<bool>) {
        let prepared = (|| {
            let effects = self.0.services.hosts.need::<OwnedEffectsHost>(Batch::D1).map_err(|_| thrown("Native close effects are unavailable"))?;
            let generation = self.0.services.views.renderer_generation(view).ok_or_else(|| thrown("The diagram renderer is unavailable"))?;
            let destination = effects.capture_owner(view, &generation).map_err(thrown)?;
            let (document, dirty, writable) = self.with_state(|state| {
                let entry = state.views.get(&view)?;
                Some((entry.document.clone()?, entry.dirty, entry.may_write()))
            }).ok_or_else(|| thrown("The diagram is no longer open"))?;
            let scope = OwnedSave::new(origin, destination, cancel, document, effects)?;
            Ok::<_, IpcError>((scope, dirty && writable))
        })();
        let (scope, dirty) = match prepared { Ok(value) => value, Err(error) => return then(Err(error)) };
        let origin = match scope.authority.renderer() { Ok(owner) => owner.clone(), Err(error) => return then(Err(error)) };
        if !dirty {
            let result = scope.effect(|| Ok(true));
            then(result);
            scope.pending.cancel();
            return;
        }
        let id = self.0.shell.random_uuid();
        if self.with_state(|state| state.view(view).owned_closes.contains_key(&id)) {
            return then(Err(thrown("Duplicate diagram close request")));
        }
        let instance = self.0.id;
        let mut subscriptions = Vec::new();
        let mut losses = vec![scope.cancel.clone(), scope.document.loss_token()];
        for owner in [&origin, &scope.destination] { if let Some(loss) = owner.loss_token() { losses.push(loss); } }
        for loss in losses {
            let pending = scope.pending.clone();
            let core = self.0.services.core.clone();
            let request = id.clone();
            subscriptions.push(loss.on_cancel(move || {
                pending.cancel();
                core.post(Box::new(move || {
                    if let Some(views) = super::super::lookup(instance) {
                        views.finish_owned_close(view, &request, Err(thrown("The close owner was released")));
                    }
                }));
            }));
        }
        self.with_state(|state| { state.view(view).owned_closes.insert(id.clone(), PendingClose { scope: scope.clone(), then, _subscriptions: subscriptions }); });
        let options = shell_contract::hosts::MessageBox {
            kind: Some("warning".into()), message: self.tm("closeUnsavedMsg"), detail: Some(self.tm("closeUnsavedDetail")),
            buttons: vec![self.tm("btnSave"), self.tm("btnDontSave"), self.tm("btnCancel")],
            default_id: Some(0), cancel_id: Some(2), ..Default::default()
        };
        let choice = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(2));
        let selected = choice.clone();
        let dialogs = self.0.dialogs.clone();
        let core = self.0.services.core.clone();
        let completion_scope = scope.clone();
        scope.effects.post_ui_owned(origin.clone(), scope.destination.clone(), scope.cancel.clone(), scope.pending.clone(),
            Box::new(move || { selected.store(dialogs.message(parent, &options).response, std::sync::atomic::Ordering::SeqCst); Ok(()) }),
            Box::new(move |result| {
                core.post(Box::new(move || {
                    let Some(views) = super::super::lookup(instance) else { return; };
                    if let Err(error) = result { return views.finish_owned_close(view, &id, Err(thrown(error))); }
                    if let Err(error) = completion_scope.check() { return views.finish_owned_close(view, &id, Err(error)); }
                    match choice.load(std::sync::atomic::Ordering::SeqCst) {
                        2 => views.finish_owned_close(view, &id, Ok(false)),
                        1 => views.discard_owned_close(view, id, completion_scope),
                        _ => {
                            let close_id = id.clone();
                            views.request_owned_save_in(view, SaveMode::Save, None, Some(origin.clone()), completion_scope.cancel.clone(), Some(completion_scope),
                                Box::new(move |result| {
                                    if let Some(views) = super::super::lookup(instance) { views.finish_owned_close(view, &close_id, result); }
                                }));
                        }
                    }
                }));
            }));
    }

    fn discard_owned_close(&self, view: ViewId, id: String, scope: OwnedSave) {
        let owned = self.with_state(|state| state.recovery.get(&view).map(|entry| entry.owned.values().cloned().collect::<Vec<_>>()).unwrap_or_default());
        let shared = self.0.recovery_state.clone();
        let dir = self.user_data_dir();
        let core = self.0.services.core.clone();
        let instance = self.0.id;
        self.0.services.workers.spawn(Box::new(move || {
            let result = with_recovery(&shared, &dir, |store| {
                for identity in owned {
                    let _ = store.delete_copy_authorized(&identity,
                        main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason::DontSave,
                        &mut |effect| scope.effect(|| { effect(); Ok(()) }).map_err(|error| main_core::node_fs::NodeError { code: None, message: error.message }));
                    scope.check()?;
                }
                scope.check()
            });
            core.post(Box::new(move || {
                let Some(views) = super::super::lookup(instance) else { return; };
                let result = result.and_then(|()| scope.effect(|| {
                    Ok(views.with_state(|state| {
                        let generation = state.next_generation();
                        state.recovery_of(view).dropped = Some(generation);
                        generation
                    }))
                }));
                let generation = match result { Ok(generation) => generation, Err(error) => return views.finish_owned_close(view, &id, Err(error)) };
                let mut hold = scope.clone();
                // This is the original recovery hold, not a child save or its parent close.
                hold.pending = CancelToken::default();
                views.after(super::super::recovery::DONT_SAVE_HOLD, move |views| {
                    let held = hold.effect(|| Ok(views.with_state(|state| {
                        match state.recovery.get_mut(&view) {
                            Some(entry) if entry.dropped == Some(generation) => { entry.dropped = None; true }
                            _ => false,
                        }
                    })));
                    if matches!(held, Ok(true)) {
                        let _ = hold.emit(&hold.destination, "diagram:recovery-reset", vec![]);
                    }
                    hold.pending.cancel();
                });
                views.finish_owned_close(view, &id, Ok(true));
            }));
        }));
    }
}
