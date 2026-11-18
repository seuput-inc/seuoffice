use super::*;
use shell_contract::WindowId;
use crate::mindmap::app::{DONT_SAVE_HOLD, TimerJob};

pub(in crate::mindmap) struct PendingClose { request: OwnedSave, done: Then<bool>, _losses: Vec<CancelSubscription> }

impl MindmapApp {
    pub(in crate::mindmap) fn request_close_owned(&self, view: ViewId, parent: Option<WindowId>, origin: NativeRequestOwner, cancel: CancelToken, done: Then<bool>) {
        if !self.is_alive(view) { return done(Ok(true)); }
        let request = match self.capture_save(view, origin, cancel) { Ok(request) => request, Err(error) => return done(Err(error)) };
        if !self.may_write(view) || !self.is_dirty(view) { return done(request.effect(|| Ok(true))); }
        let id = self.new_gen(); let mut losses = Vec::new();
        for loss in [request.authority.loss_token(), request.destination.loss_token(), Some(request.cancel.clone()), Some(request.document.loss_token())].into_iter().flatten() {
            let pending = request.pending.clone(); let core = self.0.services.core.clone();
            losses.push(loss.on_cancel(move || { pending.cancel(); core.post(Box::new(move || crate::mindmap::with_app(|app| app.finish_owned_close(id, Err(thrown("The close owner was released")))))); }));
        }
        self.0.state.borrow_mut().owned_closes.insert(id, PendingClose { request: request.clone(), done, _losses: losses });
        let options = MessageBox { kind: Some("warning".into()), message: self.tm("closeUnsavedMsg"), detail: Some(self.tm("closeUnsavedDetail")), buttons: vec![self.tm("btnSave"), self.tm("btnDontSave"), self.tm("btnCancel")], default_id: Some(0), cancel_id: Some(2), ..Default::default() };
        let core = self.0.services.core.clone();
        request.message_dialog(&self.0.dialogs, parent, options, Box::new(move |answer| core.post(Box::new(move || crate::mindmap::with_app(|app| match answer {
            Err(error) => app.finish_owned_close(id, Err(thrown(error))),
            Ok(answer) if answer.response == 2 => app.finish_owned_close(id, Ok(false)),
            Ok(answer) if answer.response == 1 => app.discard_owned_close(id),
            Ok(_) => app.save_owned_close(id),
        })))));
    }

    fn save_owned_close(&self, id: u64) {
        let request = self.0.state.borrow().owned_closes.get(&id).map(|close| close.request.clone()); let Some(request) = request else { return };
        let app = self.clone();
        let origin = match request.authority.renderer() { Ok(origin) => origin.clone(), Err(error) => return self.finish_owned_close(id, Err(error)) };
        self.request_save_owned(request.destination.view, SaveMode::Save, origin, request.cancel, None, Box::new(move |answer| app.finish_owned_close(id, answer)));
    }

    fn discard_owned_close(&self, id: u64) {
        let request = self.0.state.borrow().owned_closes.get(&id).map(|close| close.request.clone()); let Some(request) = request else { return };
        let view = request.destination.view;
        let revision = match MapRevision::capture(self, &request) { Ok(revision) => revision, Err(error) => return self.finish_owned_close(id, Err(error)) };
        let copies = self.0.state.borrow().recovery.get(&view).map(|recovery| recovery.owned.iter().map(|(_, identity)| identity.clone()).collect::<Vec<_>>()).unwrap_or_default();
        let shared = self.0.recovery_state.clone(); let directory = self.user_data_dir(); let admission = revision.admission(self, &request);
        self.offload(move || crate::owned_recovery::delete_copies(&shared, &directory, &copies, RecoveryDeleteReason::DontSave, &admission).map_err(|error| thrown(error.message)), move |app, answer| {
            let answer = answer.and_then(|_| revision.effect(app, &request, || {
                let hold = app.new_gen();
                app.0.state.borrow_mut().recovery.entry(view).or_default().dropped = Some(hold);
                app.0.state.borrow_mut().owned_holds.insert(hold, request.destination.clone());
                app.schedule(DONT_SAVE_HOLD, TimerJob::OwnedDropHold(hold));
                true
            }));
            app.finish_owned_close(id, answer);
        });
    }

    fn finish_owned_close(&self, id: u64, answer: Result<bool, IpcError>) {
        let close = self.0.state.borrow_mut().owned_closes.remove(&id); let Some(close) = close else { return };
        let answer = answer.and_then(|ok| close.request.effect(|| Ok(ok)));
        close.request.pending.cancel(); (close.done)(answer);
    }

    pub(in crate::mindmap) fn owned_hold_ended(&self, hold: u64) {
        let owner = self.0.state.borrow_mut().owned_holds.remove(&hold); let Some(owner) = owner else { return };
        let Some(lease) = owner.context.document_lease() else { return };
        let current = self.0.state.borrow().documents.get(&owner.view).is_some_and(|document| document.same_generation(lease));
        if !current { return; }
        let Some(effects) = self.0.services.hosts.get::<OwnedEffectsHost>() else { return };
        let Some(lifetime) = owner.loss_token() else { return };
        let accepted = effects.with_owned_effect(&owner, &owner, &lifetime, &lifetime, &mut || {
            let mut state = self.0.state.borrow_mut();
            let Some(recovery) = state.recovery.get_mut(&owner.view) else { return Err("The recovery hold was released".into()) };
            if recovery.dropped != Some(hold) { return Err("The recovery hold was replaced".into()); }
            recovery.dropped = None; Ok(())
        });
        if accepted.is_ok() { let _ = effects.emit_owned(&owner, &owner, &lifetime, &lifetime, "mindmap:recovery-reset", Vec::new()); }
    }
}
