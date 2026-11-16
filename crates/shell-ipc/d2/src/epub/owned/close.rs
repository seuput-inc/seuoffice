use super::*;

pub(in crate::epub) struct PendingClose {
    scope: Scope,
    then: Then<bool>,
    _subscriptions: Vec<CancelSubscription>,
}

impl PendingClose {
    pub(in crate::epub) fn cancel_and_finish(self) {
        self.scope.pending.cancel();
        (self.then)(Err(kit::error("The book view was closed")));
    }
}

impl Epub {
    fn finish_owned_close(&self, view: ViewId, id: &str, result: Result<bool, IpcError>) {
        let close = self
            .with_view(view, |entry| entry.owned_closes.remove(id))
            .flatten();
        if let Some(close) = close {
            let result = close
                .scope
                .effect(|| Ok(result))
                .map_err(kit::error)
                .and_then(|result| result);
            (close.then)(result);
            close.scope.pending.cancel();
        }
    }

    pub(in crate::epub) fn request_owned_close(
        self: &Rc<Self>,
        view: ViewId,
        parent: Option<WindowId>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    ) {
        let prepared = (|| -> Result<_, IpcError> {
            let effects = self
                .services
                .hosts
                .need::<OwnedEffectsHost>(Batch::D2)
                .map_err(|_| kit::error("Native close effects are unavailable"))?;
            let generation = self
                .services
                .views
                .renderer_generation(view)
                .ok_or_else(|| kit::error("The book renderer is unavailable"))?;
            let mut destination = effects
                .capture_owner(view, &generation)
                .map_err(kit::error)?;
            let document = self
                .view(view, |entry| entry.document.clone())
                .flatten()
                .ok_or_else(|| kit::error("The book document is unavailable"))?;
            if destination
                .context
                .document_lease()
                .is_some_and(|expected| !expected.same_generation(&document))
            {
                return Err(kit::error("The book document was replaced"));
            }
            destination.context = destination.context.with_document_lease(document.clone());
            let scope = Scope {
                authority: SaveAuthority::Renderer(origin),
                destination,
                cancel,
                pending: CancelToken::default(),
                document,
                effects,
                target: String::new(),
                native_destination: None,
                additional: Vec::new(),
                callback_owner: None,
            };
            scope.check().map_err(kit::error)?;
            Ok((scope, self.is_dirty(view)))
        })();
        let (scope, dirty) = match prepared {
            Ok(value) => value,
            Err(error) => return then(Err(error)),
        };
        let origin = match &scope.authority {
            SaveAuthority::Renderer(origin) => origin.clone(),
            SaveAuthority::Native(_) => {
                return then(Err(kit::error("The close owner is not a renderer")));
            }
        };
        if !dirty {
            let result = scope.effect(|| Ok(true)).map_err(kit::error);
            then(result);
            scope.pending.cancel();
            return;
        }
        let id = self.shell.random_token();
        if self
            .view(view, |entry| entry.owned_closes.contains_key(&id))
            .unwrap_or(false)
        {
            return then(Err(kit::error("Duplicate book close request")));
        }
        let mut subscriptions = Vec::new();
        let mut losses = vec![scope.cancel.clone(), scope.document.loss_token()];
        for owner in [&origin, &scope.destination] {
            if let Some(loss) = owner.loss_token() {
                losses.push(loss);
            }
        }
        for loss in losses {
            let pending = scope.pending.clone();
            let weak = Rc::downgrade(self);
            let request = id.clone();
            let notify = kit::back_on_core(&self.services, move |()| {
                if let Some(epub) = weak.upgrade() {
                    epub.finish_owned_close(
                        view,
                        &request,
                        Err(kit::error("The close owner was released")),
                    );
                }
            });
            subscriptions.push(loss.on_cancel(move || {
                pending.cancel();
                notify(());
            }));
        }
        self.with_view(view, |entry| {
            entry.owned_closes.insert(
                id.clone(),
                PendingClose {
                    scope: scope.clone(),
                    then,
                    _subscriptions: subscriptions,
                },
            );
        });
        let lang = self.lang();
        let options = MessageBox {
            kind: Some("warning".into()),
            message: tm(lang, "closeUnsavedMsg").into(),
            detail: Some(tm(lang, "closeUnsavedDetail").into()),
            buttons: vec![
                tm(lang, "btnSave").into(),
                tm(lang, "btnDontSave").into(),
                tm(lang, "btnCancel").into(),
            ],
            default_id: Some(0),
            cancel_id: Some(2),
            ..Default::default()
        };
        let choice = Arc::new(std::sync::atomic::AtomicUsize::new(2));
        let selected = choice.clone();
        let dialogs = self.dialogs.clone();
        let weak = Rc::downgrade(self);
        let completion_scope = scope.clone();
        let close_origin = origin.clone();
        let done = kit::back_on_core(&self.services, move |result: Result<(), String>| {
            let Some(epub) = weak.upgrade() else {
                return;
            };
            if let Err(error) = result {
                return epub.finish_owned_close(view, &id, Err(kit::error(error)));
            }
            if let Err(error) = completion_scope.check() {
                return epub.finish_owned_close(view, &id, Err(kit::error(error)));
            }
            match choice.load(std::sync::atomic::Ordering::SeqCst) {
                2 => epub.finish_owned_close(view, &id, Ok(false)),
                1 => epub.finish_owned_close(view, &id, Ok(true)),
                _ => {
                    let completion = epub.clone();
                    epub.begin_owned_in(
                        view,
                        SaveMode::Save,
                        None,
                        Some(close_origin),
                        completion_scope.cancel.clone(),
                        Some(completion_scope),
                        Box::new(move |result| completion.finish_owned_close(view, &id, result)),
                    );
                }
            }
        });
        scope.effects.post_ui_owned(
            origin.clone(),
            scope.destination.clone(),
            scope.cancel.clone(),
            scope.pending.clone(),
            Box::new(move || {
                selected.store(
                    dialogs.message(parent, &options).response,
                    std::sync::atomic::Ordering::SeqCst,
                );
                Ok(())
            }),
            done,
        );
    }
}
