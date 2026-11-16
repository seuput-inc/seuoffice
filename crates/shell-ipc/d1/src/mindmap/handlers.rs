//! `mindmap-main.ts`'s `registerMindmapIpc`, `handoff-main.ts`,
//! `seed-main.ts` and the reply channels of `mcp-queries.ts`.

use std::collections::HashSet;
use std::fs;

use jscompat::node_path::host::basename;
use jscompat::{JsObject, JsonValue};
use main_core::packages::electron_utils::dialog_memory::{
    FallbackDir, show_save_dialog_with_memory,
};
use main_core::packages::electron_utils::safe_external_url::{
    SafeExternalUrlOptions, safe_external_url,
};
use shell_contract::ipc::{CallContext, Reply};
use shell_contract::{IpcValue, ViewId};

use super::app::{Answer, Ask, MindmapApp, Wait};
use super::hosts::OpenStatus;
use super::util::{has_key, js_string, one_line, slice, utf16_len};
use crate::host::HostDialog;
use crate::route::{self, Route};
use crate::value::{Args, as_bytes, buffer, field, num, obj, string, thrown};

/// The largest `.mm` read for an import; a FreeMind map is plain XML
const MAX_FREEMIND_BYTES: u64 = 32 * 1024 * 1024;
/// more attachments than a panel would ever hold are not a real report
const MAX_REPORTED_PATHS: usize = 256;
const MAX_PATH_CHARS: usize = 4096;

/// The links a map opens: `mailto:` stays openable on purpose
const LINK_PROTOCOLS: [&str; 3] = ["http:", "https:", "mailto:"];

fn is_freemind(path: &str) -> bool {
    path.to_lowercase().ends_with(".mm")
}

/// The path picked in a save dialog over the view's window, or `None` when it
/// was cancelled or picked nothing (`picked.canceled || !picked.filePath`)
pub(super) fn pick_save_path(
    app: &MindmapApp,
    view: ViewId,
    options: &JsObject<JsonValue>,
    fallback: Option<&FallbackDir<'_>>,
) -> Option<String> {
    let parent = app.window_of(view);
    let mut memory = app
        .0
        .shared
        .dialog_memory
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut dialog = HostDialog(&*app.0.dialogs);
    let picked =
        show_save_dialog_with_memory(&mut memory, &mut dialog, parent.as_ref(), options, fallback);
    let JsonValue::Object(picked) = &picked else {
        return None;
    };
    if picked.get("canceled").is_some_and(JsonValue::truthy) {
        return None;
    }
    match picked.get("filePath") {
        Some(JsonValue::String(path)) if !path.is_empty() => Some(path.clone()),
        _ => None,
    }
}

/// `importFreeMind`: a `.mm` for the renderer to import: its bytes, read and
/// never written
fn import_free_mind(app: &MindmapApp, view: ViewId, path: &str) -> Reply {
    let read = fs::metadata(path)
        .ok()
        .filter(|meta| meta.len() <= MAX_FREEMIND_BYTES)
        .and_then(|_| fs::read(path).ok());
    let Some(bytes) = read else {
        app.refuse_writes(view);
        return Ok(obj([("status", string("unreadable"))]));
    };
    let title = {
        let name = basename(path, None);
        name[..name.len() - 3].to_owned()
    };
    {
        let mut state = app.0.state.borrow_mut();
        state.unopened.remove(&view);
        state.save_path.remove(&view);
        state
            .imported
            .insert(view, (path.to_owned(), title.clone()));
    }
    Ok(obj([
        ("status", string("import")),
        ("importer", string("freemind")),
        ("bytes", buffer(bytes)),
        ("title", string(title)),
    ]))
}

fn open_file(app: &MindmapApp, cx: &CallContext, args: Args) -> Reply {
    let view = cx.sender.view;
    let granted = match args.str(0) {
        Some(path) => app
            .0
            .state
            .borrow()
            .allowed
            .get(&view)
            .is_some_and(|allowed: &HashSet<String>| allowed.contains(path)),
        None => false,
    };
    let Some(path) = args.str(0).filter(|_| granted) else {
        return Err(thrown("mindmap: path not granted to this view"));
    };
    if let Some(response) = app.open_recovery_file(cx, path)? {
        return Ok(response);
    }
    let _opened_owner = app.replace_save_document(cx)?;
    if is_freemind(path) {
        return import_free_mind(app, view, path);
    }
    // A recovery copy left for this file is settled before any of the map is loaded.
    let restored = app.resolve_view_open(view, path, app.window_of(view));
    let bytes = fs::read(path).ok();
    if let Some(restored) = restored {
        let copy = app.0.engine.open_xmind(&restored).map_err(thrown)?;
        if copy.status == OpenStatus::Ok {
            // the restored map is written only by a save; the copy is the whole file
            let original_opens = match bytes.as_ref() {
                Some(bytes) => {
                    app.0.engine.open_xmind(bytes).map_err(thrown)?.status == OpenStatus::Ok
                }
                None => false,
            };
            app.0.state.borrow_mut().unopened.remove(&view);
            if original_opens {
                // the file still opens: it stays the save target
                app.0
                    .state
                    .borrow_mut()
                    .save_path
                    .insert(view, path.to_owned());
                app.remember_disk_state(view, path);
                return Ok(obj([
                    ("status", string("ok")),
                    ("bytes", buffer(restored)),
                    ("restored", string("file")),
                ]));
            }
            // The file is gone or no longer opens, so it is not written over: the
            // map is untitled, its first save offers the old place, and its copies
            // keep replacing the restored one.
            app.0.state.borrow_mut().save_path.remove(&view);
            app.view_restored_untitled(view, path);
            return Ok(obj([
                ("status", string("ok")),
                ("bytes", buffer(restored)),
                ("restored", string("untitled")),
            ]));
        }
    }
    let Some(bytes) = bytes else {
        app.refuse_writes(view);
        return Ok(obj([("status", string("unreadable"))]));
    };
    let opened = app.0.engine.open_xmind(&bytes).map_err(thrown)?;
    if opened.status != OpenStatus::Ok {
        app.refuse_writes(view);
        return Ok(obj([("status", string(opened.status.as_str()))]));
    }
    // Seen open, here: only now is the path a save target. This also covers a
    // reload of a view whose file opens again after it was replaced on disk.
    {
        let mut state = app.0.state.borrow_mut();
        state.unopened.remove(&view);
        state.save_path.insert(view, path.to_owned());
    }
    app.remember_disk_state(view, path);
    Ok(obj([("status", string("ok")), ("bytes", buffer(bytes))]))
}

fn open_link(app: &MindmapApp, args: Args) -> Reply {
    let options = SafeExternalUrlOptions {
        allowed_protocols: Some(LINK_PROTOCOLS.map(String::from).to_vec()),
    };
    let Some(target) = safe_external_url(args.str(0), Some(&options)) else {
        return Ok(IpcValue::Bool(false));
    };
    Ok(IpcValue::Bool(
        app.0.os_shell.open_external(&target).is_ok(),
    ))
}

fn untitled_copy_title(app: &MindmapApp, view: ViewId, offered: Option<&str>) -> String {
    if let Some(from_shell) = app.0.shell.recovery_title(view) {
        let trimmed = jscompat::trim(&from_shell);
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let name = offered
        .map(|offered| slice(&one_line(offered), 0, 80))
        .unwrap_or_default();
    if name.is_empty() {
        app.tm("untitledFile")
    } else {
        name
    }
}

fn recovery_copy(app: &MindmapApp, cx: &CallContext, args: Args) -> Reply {
    use main_core::packages::electron_utils::recovery_store::RecoveryWriteResult;
    let view = cx.sender.view;
    let skipped = || Ok(string("skipped"));
    // nothing unsaved (a save came first), or a view whose file did not open
    if !app.may_write(view) || !app.is_dirty(view) {
        return skipped();
    }
    let request = args.get(0);
    let Some(bytes) = as_bytes(field(request, "bytes")) else {
        return skipped();
    };
    let offered = match field(request, "name") {
        IpcValue::String(name) => Some(name.as_str()),
        _ => None,
    };
    let save_path = app.file_path(view);
    match app.write_view_copy(view, save_path.as_deref(), bytes, || {
        untitled_copy_title(app, view, offered)
    }) {
        Ok(RecoveryWriteResult::Written) => Ok(string("written")),
        Ok(RecoveryWriteResult::Superseded) => Ok(string("superseded")),
        Err(error) => Err(thrown(error.message)),
    }
}

/// `reportedPaths`: the paths of a run-started report, or `None` when the report is not one
fn reported_paths(report: &IpcValue) -> Option<(String, Vec<String>)> {
    let IpcValue::String(id) = field(report, "id") else {
        return None;
    };
    if id.is_empty() {
        return None;
    }
    let IpcValue::Array(attached) = field(report, "attachedPaths") else {
        return None;
    };
    let paths: Vec<String> = attached
        .iter()
        .take(MAX_REPORTED_PATHS)
        .filter_map(|item| match item {
            IpcValue::String(p) if !p.is_empty() && utf16_len(p) <= MAX_PATH_CHARS => {
                Some(p.clone())
            }
            _ => None,
        })
        .collect();
    (!paths.is_empty()).then(|| (id.clone(), paths))
}

/// the settings of a read of the map the renderer answered (`question.parse`)
fn parse_reply(kind: Ask, reply: &IpcValue) -> Option<Result<Answer, String>> {
    if !matches!(reply, IpcValue::Object(_)) {
        return None;
    }
    let error_of =
        |reply: &IpcValue| has_key(reply, "error").then(|| Err(js_string(field(reply, "error"))));
    match kind {
        Ask::Outline => match field(reply, "text") {
            IpcValue::String(text) if has_key(reply, "text") => {
                Some(Ok(Answer::Text(text.clone())))
            }
            _ => error_of(reply),
        },
        Ask::Bytes => {
            if has_key(reply, "bytes") {
                return Some(match field(reply, "bytes") {
                    IpcValue::Bytes(_, bytes) => Ok(Answer::Bytes(bytes.clone())),
                    _ => Err("the map sent no bytes".into()),
                });
            }
            error_of(reply)
        }
    }
}

pub(super) fn routes(app: &MindmapApp) -> Vec<Route> {
    let mut routes = Vec::new();

    let a = app.clone();
    routes.push(route::invoke_later(
        "mindmap:consume-pending",
        move |cx, _args, reply| {
            let reply = match super::saved_open::dispatch(
                &a,
                cx,
                super::saved_open::WaitKind::Pending,
                reply,
            ) {
                Ok(()) => return,
                Err(reply) => reply,
            };
            let result = a.recovery_pending_path(cx).map(|path| {
                path.unwrap_or_else(|| match a.0.state.borrow().open_path.get(&cx.sender.view) {
                    Some(path) => string(path.clone()),
                    None => IpcValue::Null,
                })
            });
            reply.reply(result);
        },
    ));
    let a = app.clone();
    routes.push(route::invoke(
        "mindmap:recovery-initialized",
        move |cx, args| super::recovery_open::initialized(&a, cx, args),
    ));

    let a = app.clone();
    routes.push(route::invoke_later(
        "mindmap:open-file",
        move |cx, args, reply| {
            let path = args.str(0).unwrap_or("").to_owned();
            let reply = match super::saved_open::dispatch(
                &a,
                cx,
                super::saved_open::WaitKind::Open(path),
                reply,
            ) {
                Ok(()) => return,
                Err(reply) => reply,
            };
            reply.reply(open_file(&a, cx, args));
        },
    ));

    let a = app.clone();
    routes.push(route::invoke_later(
        "mindmap:save",
        move |cx, args, responder| super::owned_save::save(&a, cx, args, responder),
    ));

    let a = app.clone();
    routes.push(route::invoke_later(
        "mindmap:save-into-project",
        move |cx, args, responder| super::owned_save::save_into_project(&a, cx, args, responder),
    ));

    let a = app.clone();
    routes.push(route::invoke("mindmap:open-link", move |_cx, args| {
        open_link(&a, args)
    }));

    let a = app.clone();
    routes.push(route::send("mindmap:dirty-changed", move |cx, args| {
        let view = cx.sender.view;
        let mut state = a.0.state.borrow_mut();
        let revision = state.revisions.entry(view).or_default();
        *revision = revision.saturating_add(1);
        if matches!(args.get(0), IpcValue::Bool(true)) && !state.unopened.contains_key(&view) {
            state.dirty.insert(view);
        } else {
            state.dirty.remove(&view);
        }
        Ok(())
    }));

    let a = app.clone();
    routes.push(route::send("mindmap:close-save-result", move |cx, args| {
        let waiter = a.0.state.borrow_mut().close_waiters.take(cx.sender.view);
        if let Some(waiter) = waiter {
            waiter(Wait::Answer(matches!(args.get(0), IpcValue::Bool(true))));
        }
        Ok(())
    }));

    // safety net for menu saves the renderer declined without invoking save()
    // (still loading, or a file that did not open): the save handler itself
    // resolves the normal path
    let a = app.clone();
    routes.push(route::send("mindmap:save-request-ack", move |cx, args| {
        if let Some(id) = args.str(1) {
            return a.save_ack_owned(
                cx,
                id,
                matches!(args.get(0), IpcValue::Bool(true)),
                args.str(2),
            );
        }
        let waiter = a.0.state.borrow_mut().save_waiters.take(cx.sender.view);
        if let Some(waiter) = waiter {
            waiter(Wait::Answer(matches!(args.get(0), IpcValue::Bool(true))));
        }
        Ok(())
    }));

    let a = app.clone();
    routes.push(route::send(
        "mindmap:make-from-document",
        move |cx, _args| {
            a.0.shell.make_from_document(cx.clone());
            Ok(())
        },
    ));

    let a = app.clone();
    routes.push(route::invoke("mindmap:recovery-copy", move |cx, args| {
        recovery_copy(&a, cx, args)
    }));

    let a = app.clone();
    routes.push(route::send(
        "mindmap:recovery-copy-now-done",
        move |cx, args| {
            let Some(generation) = args.num(0) else {
                return Ok(());
            };
            if !generation.is_finite() || generation < 0.0 || generation.fract() != 0.0 {
                return Ok(());
            }
            let result = match args.get(1) {
                IpcValue::Null => Ok(()),
                IpcValue::String(message) => Err(main_core::shell::renderer_disposal::Failure(
                    message.clone(),
                )),
                _ => return Ok(()),
            };
            a.finish_copy_now(cx.sender.view, Some(generation as u64), result);
            Ok(())
        },
    ));

    let a = app.clone();
    routes.push(route::invoke("mindmap:handoff-take", move |cx, _args| {
        let view = cx.sender.view;
        if a.opens_file(view) {
            return Ok(IpcValue::Null);
        }
        let consumer = super::owned_save::native_owner(cx)?;
        let Some(handoff) =
            a.0.shell
                .take_generate_handoff_owned(&consumer, &cx.cancel)
                .map_err(thrown)?
        else {
            return Ok(IpcValue::Null);
        };
        a.0.state
            .borrow_mut()
            .taken_handoff
            .insert(view, handoff.id.clone());
        Ok(obj([
            ("id", string(handoff.id)),
            (
                "sourcePaths",
                IpcValue::Array(
                    handoff
                        .materials
                        .iter()
                        .map(|m| string(m.path.clone()))
                        .collect(),
                ),
            ),
            ("prompt", string(handoff.text)),
        ]))
    }));

    let a = app.clone();
    routes.push(route::send(
        "mindmap:handoff-run-started",
        move |cx, args| {
            let view = cx.sender.view;
            let Some((id, paths)) = reported_paths(args.get(0)) else {
                return Ok(());
            };
            if a.0.state.borrow().taken_handoff.get(&view) != Some(&id) {
                return Ok(());
            }
            // the store keeps the first run with at least one handed-over file attached
            if a.0
                .shell
                .report_generate_run_started_owned(
                    &super::owned_save::native_owner(cx)?,
                    &cx.cancel,
                    &paths,
                )
                .map_err(thrown)?
            {
                a.0.state.borrow_mut().taken_handoff.remove(&view);
            }
            Ok(())
        },
    ));

    let a = app.clone();
    routes.push(route::invoke("mindmap:seed-take", move |cx, _args| {
        if let Some(seed) = a.take_recovery_seed(cx)? {
            return Ok(seed);
        }
        let view = cx.sender.view;
        if a.opens_file(view) {
            return Ok(IpcValue::Null);
        }
        Ok(a.take_seed_for(cx)?
            .map_or(IpcValue::Null, |seed| seed.to_value()))
    }));

    for (channel, kind) in [
        ("mindmap:outline-result", Ask::Outline),
        ("mindmap:bytes-result", Ask::Bytes),
    ] {
        let a = app.clone();
        routes.push(route::send(channel, move |cx, args| {
            let view = cx.sender.view;
            if !a.has_pending_ask(kind, view) {
                return Ok(());
            }
            if let Some(answer) = parse_reply(kind, args.get(0)) {
                a.settle_ask(kind, view, None, answer);
            }
            Ok(())
        }));
    }

    let _ = num;
    routes
}
