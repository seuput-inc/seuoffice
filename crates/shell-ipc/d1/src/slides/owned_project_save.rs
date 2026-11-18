use super::*;

fn skipped() -> IpcValue { obj([("ok", IpcValue::Bool(false)), ("skipped", IpcValue::Bool(true))]) }
fn failure(error: IpcError) -> IpcValue { obj([("ok", IpcValue::Bool(false)), ("error", string(error.message))]) }

pub(in crate::slides) fn save_into_project(app: &App, cx: &CallContext, _args: Args, responder: Responder) {
    let eligible = {
        let state = app.state.borrow();
        state.session(cx.sender.view).is_some_and(|session| session.path().is_empty() && session.master_edit.is_none() && !session.core.ai_run && app.shell.can_write_new_document())
    };
    if !eligible { return responder.reply(Ok(skipped())); }
    let owner = match crate::slides::session::render_owner(cx) { Ok(owner) => owner, Err(error) => return responder.reply(Err(error)) };
    let scope = match SaveScope::capture(app, cx.sender.view, owner, cx.cancel.clone()) { Ok(scope) => scope, Err(error) => return responder.reply(Err(error)) };
    let revision = match scope.revision(app) { Ok(revision) => revision, Err(error) => return responder.reply(Err(error)) };
    let (name, password) = {
        let state = app.state.borrow();
        let Some(session) = state.slot(scope.slot) else { return responder.reply(Err(thrown("The presentation was released"))); };
        (app.project_deck_file_name(session.first_save_name.as_deref()), session.password.clone())
    };
    let bytes = app.engine.save_bytes(scope.deck).map_err(|error| thrown(error.0)).and_then(|bytes| match password {
        Some(password) => app.engine.encrypt_ooxml(&bytes, &password).map_err(|error| thrown(error.0)), None => Ok(bytes),
    });
    let bytes = match bytes { Ok(bytes) => bytes, Err(error) => return responder.reply(Ok(failure(error))) };
    let admission = scope.revision_admission(app, revision.clone()); let shell = app.shell.clone(); let request = scope.request.clone();
    let origin = match request.authority.renderer() { Ok(origin) => origin.clone(), Err(error) => return responder.reply(Err(error)) };
    let services = app.services.clone(); let index = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let outcome = shell.write_new_document_owned(&origin, &request.destination, &request.cancel, &request.pending, &request.effects, admission, &name, &bytes)
            .map(|path| path.map(|path| { let disk = record_disk_state(&path); (path, disk) }));
        crate::slides::post_to(&services, index, move |app| match outcome {
            Err(error) => responder.reply(Ok(failure(thrown(error)))),
            Ok(None) => responder.reply(scope.effect_at_revision(app, &revision, skipped)),
            Ok(Some((path, disk))) => app.project_file_written(scope, revision, path, disk, responder),
        });
    }));
}

impl App {
    fn project_file_written(&self, scope: SaveScope, revision: SaveRevision, path: String, disk: Option<SaveDiskState>, responder: Responder) {
        if let Err(error) = scope.effect_at_revision(self, &revision, || ()) { return responder.reply(Err(error)); }
        let id = self.shell.random_uuid(); let render_scope = scope.clone();
        let done = Box::new(move |app: &App, result: Result<(), IpcError>| {
            let reply = result.and_then(|()| {
                render_scope.check(app)?;
                let state = app.state.borrow(); let session = state.slot(render_scope.slot).ok_or_else(|| thrown("The presentation was released"))?;
                Ok(obj([("ok", IpcValue::Bool(true)), ("path", string(session.path())), ("slides", app.render_all(session, &render_scope.render_owner)?)]))
            });
            responder.reply(Ok(reply.unwrap_or_else(failure)));
        });
        self.state.borrow_mut().owned_writes.insert(id.clone(), PendingWrite { scope, previous: revision.path, path, meta_rev: revision.meta_rev, op_seq: revision.op_seq, remaining: 0, failure: None, done });
        self.deck_written(&id, Ok(disk));
    }
}
