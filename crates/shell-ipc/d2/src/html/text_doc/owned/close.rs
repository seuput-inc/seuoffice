use super::*;

impl TextDocs {
    pub(crate) fn close_options(&self) -> MessageBoxOptions {
        let lang = self.lang();
        MessageBoxOptions {
            kind: "warning",
            title: None,
            message: tr(
                &lang,
                "This document has unsaved changes.",
                "이 문서에 저장하지 않은 변경 사항이 있습니다.",
            ),
            detail: Some(tr(
                &lang,
                "Do you want to save them before closing?",
                "닫기 전에 저장하시겠습니까?",
            )),
            buttons: vec![
                tr(&lang, "Save", "저장"),
                tr(&lang, "Don't Save", "저장 안 함"),
                tr(&lang, "Cancel", "취소"),
            ],
            default_id: 0,
            cancel_id: Some(2),
            no_link: Some(true),
        }
    }

    pub(crate) fn request_close_owned(
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
        let request = match self.capture_close(view, origin, cancel) {
            Err(error) => return then(Err(kit::error(error))),
            Ok(request) => request,
        };
        if let Err(error) = request.check_document(self, view) {
            return then(Err(kit::error(error)));
        }
        if !self.is_dirty(view) {
            return then(Ok(true));
        }
        let docs = Rc::clone(self);
        let retained = request.clone();
        let done = kit::back_on_core(
            &self.services,
            move |answer: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
                let request = retained;
                if let Err(error) = request.check_document(&docs, view) {
                    return then(Err(kit::error(error)));
                }
                match answer {
                    Err(error) => then(Err(kit::error(error))),
                    Ok(answer) if answer.response == 2 => then(Ok(false)),
                    Ok(answer) if answer.response == 1 => docs.discard_owned(view, request, then),
                    Ok(_) => {
                        let Some(origin) = request.origin().cloned() else {
                            return then(Err(kit::error("close save has no renderer origin")));
                        };
                        let destination = request.destination().clone();
                        let cancel = request.cancel();
                        docs.begin_owned(
                            view,
                            SaveMode::Save,
                            None,
                            origin,
                            cancel,
                            Some(destination),
                            true,
                            Box::new(move |result| {
                                let _request = request;
                                then(result)
                            }),
                        );
                    }
                }
            },
        );
        let Some(origin) = request.origin().cloned() else {
            return done(Err("close dialog has no renderer origin".into()));
        };
        self.dialogs.message_owned(
            parent,
            message_box(&self.close_options()),
            origin,
            request.destination().clone(),
            request.cancel(),
            request.pending(),
            done,
        );
    }

    fn capture_close(
        &self,
        view: ViewId,
        origin: NativeRequestOwner,
        cancel: CancelToken,
    ) -> Result<OwnedSave, String> {
        let effects = self
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or("owned close effects are unavailable")?;
        let expected = self
            .services
            .views
            .renderer_generation(view)
            .ok_or("the destination renderer lifetime is unavailable")?;
        let destination = self
            .capture_document_owner(view, effects.capture_owner(view, &expected)?)
            .map_err(|error| error.message)?;
        let combined = CancelToken::default();
        let mut subscriptions = Vec::new();
        for token in [
            cancel,
            origin
                .loss_token()
                .ok_or("the originating request lifetime is unavailable")?,
            destination
                .loss_token()
                .ok_or("the destination native lifetime is unavailable")?,
        ] {
            let combined = combined.clone();
            subscriptions.push(token.on_cancel(move || combined.cancel()));
        }
        Ok(OwnedSave(Arc::new(Request {
            default_dir: std::sync::OnceLock::new(),
            id: 0,
            authority: SaveAuthority::Renderer(origin),
            destination,
            participants: Vec::new(),
            effects,
            cancel: combined.clone(),
            pending: combined,
            additional: Vec::new(),
            mode: SaveMode::Save,
            closing: true,
            claimed: std::sync::atomic::AtomicBool::new(false),
            callback: std::sync::OnceLock::new(),
            selected: std::sync::OnceLock::new(),
            target: None,
            original_save: self.save_path(view),
            original_open: self.open_path(view),
            _subscriptions: subscriptions,
        })))
    }

    fn discard_owned(self: &Rc<Self>, view: ViewId, request: OwnedSave, then: Then<bool>) {
        let identity = self.recovery.identity_of(view);
        let path = self.save_path(view);
        let docs = Rc::clone(self);
        let retained = request.clone();
        self.recovery.finish_owned(view,identity,main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason::DontSave,request,move|deleted|{
            if let Err(error)=deleted{return then(Err(kit::error(error)))}
            let Some(path)=path else{return then(retained.effect(&docs,||Ok(true)).map_err(kit::error))};
            let html=docs.ch.app=="html";let worker=retained.clone();let completed=Rc::clone(&docs);
            kit::on_worker(&docs.services,move||{
                if html{main_core::html::asset_lifecycle::discard_pending_owned_assets_authorized(&path,&mut|effect|super::super::owned_write::admit(&worker,effect)).map(|result|result.errors)}
                else{main_core::markdown::asset_lifecycle::discard_pending_owned_assets_authorized(&path,&mut|effect|super::super::owned_write::admit(&worker,effect)).map(|result|result.errors)}
            },move|discarded|{
                let discarded=match discarded{Err(error)=>return then(Err(kit::error(error.message))),Ok(discarded)=>discarded};
                if !discarded.is_empty(){eprintln!("[{}] pending asset discard incomplete: {discarded:?}",completed.ch.app);}
                then(retained.effect(&completed,||Ok(true)).map_err(kit::error));
            });
        });
    }

    pub(crate) fn handle_owned_close_result(
        &self,
        view: ViewId,
        native: Option<&NativeContext>,
        success: bool,
    ) -> bool {
        let Some(request) = self.owned_saves.borrow().get(&view).cloned() else {
            return false;
        };
        if !request.0.closing {
            return false;
        }
        if request.matches(native) && !request.0.claimed.load(std::sync::atomic::Ordering::SeqCst) {
            let accepted = success && !self.is_dirty(view) && self.save_path(view).is_some();
            let waiter = self.save_waiters.borrow_mut().remove(&view);
            if let Some(waiter) = waiter {
                waiter(accepted)
            }
        }
        true
    }
}
