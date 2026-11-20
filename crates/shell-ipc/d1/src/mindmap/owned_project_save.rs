use super::*;

fn skipped() -> IpcValue {
    obj([
        ("ok", IpcValue::Bool(false)),
        ("skipped", IpcValue::Bool(true)),
    ])
}

/// Electron answers a refused project save as `{ ok: false, error }`.
fn refused(message: impl Into<String>) -> IpcValue {
    obj([("ok", IpcValue::Bool(false)), ("error", string(message))])
}

type Written = (
    String,
    Option<main_core::packages::electron_utils::save_conflict::SaveDiskState>,
);

pub(in crate::mindmap) fn save_into_project(
    app: &MindmapApp,
    cx: &CallContext,
    args: Args,
    responder: Responder,
) {
    let view = cx.sender.view;
    if !app.0.shell.has_new_document_writer()
        || !app.may_write(view)
        || app.file_path(view).is_some()
    {
        return responder.reply(Ok(skipped()));
    }
    let value = args.get(0);
    let Some(bytes) = as_bytes(field(value, "bytes")).map(Vec::from) else {
        return responder.reply(Ok(refused("mindmap: bad save request")));
    };
    let request = match native_owner(cx)
        .and_then(|origin| app.capture_save(view, origin, cx.cancel.clone()))
    {
        Ok(request) => request,
        Err(error) => return responder.reply(Err(error)),
    };
    let revision = match MapRevision::capture(app, &request) {
        Ok(revision) => revision,
        Err(error) => return responder.reply(Err(error)),
    };
    let base = file_name_base(as_str(field(value, "defaultName")));
    let base = if base.is_empty() {
        app.tm("untitledFile")
    } else {
        base
    };
    let name = format!("{base}{MAP_EXTENSION}");
    let id = app.insert_save(request.clone(), SaveMode::Save, None, None);
    let initialized = revision
        .effect(app, &request, || {
            let mut state = app.0.state.borrow_mut();
            let pending = state
                .owned_saves
                .get_mut(&id)
                .ok_or_else(|| thrown("The project save request was released"))?;
            pending.revision = Some(revision.revision);
            pending.baseline = revision.path.clone();
            Ok::<_, IpcError>(())
        })
        .and_then(|answer| answer);
    if let Err(error) = initialized {
        return fail(app, id, responder, error);
    }
    let engine = app.0.engine.clone();
    let shell = app.0.shell.clone();
    let admission = app.admission(id);
    let worker_request = request.clone();
    let cx = cx.clone();
    app.offload(
        move || -> Result<Result<Option<Written>, String>, IpcError> {
            let opened = engine.open_xmind(&bytes).map_err(thrown)?;
            if opened.status != OpenStatus::Ok {
                return Ok(Err(format!(
                    "mindmap: refusing to write a map that would not open ({})",
                    opened.status.as_str()
                )));
            }
            Ok(shell
                .write_new_document_owned(
                    worker_request.authority.renderer()?,
                    &worker_request.destination,
                    &worker_request.cancel,
                    &worker_request.pending,
                    &worker_request.effects,
                    admission,
                    &name,
                    &bytes,
                )
                .map(|path| {
                    path.filter(|path| !path.is_empty()).map(|path| {
                        let disk = record_disk_state(&path);
                        (path, disk)
                    })
                }))
        },
        move |app, result| match result {
            Err(error) => fail(app, id, responder, error),
            Ok(Err(message)) => {
                let result = revision.effect(app, &request, || refused(message));
                app.release_direct_save(id);
                responder.reply(result);
            }
            Ok(Ok(None)) => {
                let result = revision.effect(app, &request, skipped);
                app.release_direct_save(id);
                responder.reply(result);
            }
            Ok(Ok(Some((path, disk)))) => {
                app.adopt_owned_map(cx, id, path, request, disk, responder)
            }
        },
    );
}
