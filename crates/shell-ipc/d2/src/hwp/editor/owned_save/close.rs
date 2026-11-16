use super::*;

impl Hwp {
    fn finish_close_owned(&self, view: ViewId, id: u64, result: Result<bool, IpcError>) {
        let operation = {
            let mut operations = self.owned_closes.borrow_mut();
            if !operations
                .get(&view)
                .is_some_and(|operation| operation.id == id)
            {
                return;
            }
            operations.remove(&view)
        };
        if let Some(operation) = operation {
            let then = operation.then.borrow_mut().take();
            operation.subscriptions.borrow_mut().clear();
            operation.scope.pending.cancel();
            if let Some(then) = then {
                then(result);
            }
        }
    }

    pub(super) fn cancel_close_owned(&self, view: ViewId, message: &str) {
        let id = self
            .owned_closes
            .borrow()
            .get(&view)
            .map(|operation| operation.id);
        if let Some(id) = id {
            self.finish_close_owned(view, id, Err(error(message)));
        }
    }

    pub(in crate::hwp::editor) fn request_close_owned(
        self: &Rc<Self>,
        view: ViewId,
        parent: Option<WindowId>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    ) {
        if self.is_destroyed(view) {
            return then(Ok(true));
        }
        let Some(generation) = self.services.views.renderer_generation(view) else {
            return then(Err(error("hwp: renderer is unavailable")));
        };
        let destination = match self
            .owned_effects
            .capture_owner(view, &generation)
            .and_then(|owner| {
                self.capture_hwp_owner(view, owner)
                    .map_err(|error| error.message)
            }) {
            Ok(owner) => owner,
            Err(message) => return then(Err(error(message))),
        };
        let scope = Scope {
            authority: SaveAuthority::Renderer(origin),
            participants: vec![destination.clone()],
            destination,
            cancel,
            pending: CancelToken::default(),
            additional: Vec::new(),
            effects: self.owned_effects.clone(),
            recovery: None,
            native_destination: None,
        };
        let mut snapshot = None;
        if let Err(message) = scope.effect(&mut || {
            snapshot = Some(self.get(view, |state| (state.memo_modal, state.dirty)));
            Ok(())
        }) {
            return then(Err(error(message)));
        }
        let Some((memo, dirty)) = snapshot else {
            return then(Err(error("hwp: close state was not captured")));
        };
        self.cancel_close_owned(view, "hwp: close request replaced");
        let id = kit::unique();
        let operation = Rc::new(OwnedClose {
            id,
            scope: scope.clone(),
            then: RefCell::new(Some(then)),
            subscriptions: RefCell::default(),
        });
        self.owned_closes
            .borrow_mut()
            .insert(view, operation.clone());
        for loss in [
            Some(scope.cancel.clone()),
            scope.authority_loss(),
            scope.destination.loss_token(),
        ]
        .into_iter()
        .flatten()
        {
            let pending = scope.pending.clone();
            operation
                .subscriptions
                .borrow_mut()
                .push(loss.on_cancel(move || pending.cancel()));
        }
        let hwp = self.clone();
        let expired = kit::back_on_core(&self.services, move |()| {
            hwp.finish_close_owned(view, id, Err(error("hwp: close request expired")))
        });
        operation
            .subscriptions
            .borrow_mut()
            .push(scope.pending.on_cancel(move || expired(())));
        if !memo && !dirty {
            return self.finish_close_owned(view, id, Ok(true));
        }
        let options = if memo {
            MessageBox {
                kind: Some("warning".into()),
                message: self.tm("closeMemoMsg"),
                detail: Some(self.tm("closeMemoDetail")),
                buttons: vec![self.tm("btnDiscardClose"), self.tm("btnCancel")],
                default_id: Some(1),
                cancel_id: Some(1),
                ..Default::default()
            }
        } else {
            MessageBox {
                kind: Some("warning".into()),
                message: self.tm("closeUnsavedMsg"),
                detail: Some(self.tm("closeUnsavedDetail")),
                buttons: vec![
                    self.tm("btnSave"),
                    self.tm("btnDontSave"),
                    self.tm("btnCancel"),
                ],
                default_id: Some(0),
                cancel_id: Some(2),
                ..Default::default()
            }
        };
        let hwp = self.clone();
        let dialog_scope = scope.clone();
        let done = kit::back_on_core(
            &self.services,
            move |answer: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
                if !hwp
                    .owned_closes
                    .borrow()
                    .get(&view)
                    .is_some_and(|operation| operation.id == id)
                {
                    return;
                }
                let mut discard_dirty = dirty;
                if let Err(message) = dialog_scope.effect(&mut || {
                    if memo {
                        discard_dirty = hwp.get(view, |state| state.dirty);
                    }
                    Ok(())
                }) {
                    return hwp.finish_close_owned(view, id, Err(error(message)));
                }
                let answer = match answer {
                    Ok(answer) => answer.response,
                    Err(message) => return hwp.finish_close_owned(view, id, Err(error(message))),
                };
                if (memo && answer != 0) || (!memo && answer == 2) {
                    return hwp.finish_close_owned(view, id, Ok(false));
                }
                if memo || answer == 1 {
                    if !discard_dirty {
                        return hwp.finish_close_owned(view, id, Ok(true));
                    }
                    let completed = hwp.clone();
                    return hwp.discard_recovery_owned(
                        view,
                        dialog_scope,
                        Box::new(move |result| {
                            completed.finish_close_owned(view, id, result.map(|()| true))
                        }),
                    );
                }
                let completed = hwp.clone();
                let origin = match dialog_scope.renderer_origin() {
                    Ok(origin) => origin.clone(),
                    Err(message) => {
                        return hwp.finish_close_owned(view, id, Err(error(message)));
                    }
                };
                hwp.request_save_captured(
                    view,
                    SaveMode::Save,
                    origin,
                    dialog_scope.destination,
                    dialog_scope.pending,
                    Box::new(move |result| completed.finish_close_owned(view, id, result)),
                );
            },
        );
        scope.message(&self.dialogs, parent, options, Box::new(done));
    }
}
