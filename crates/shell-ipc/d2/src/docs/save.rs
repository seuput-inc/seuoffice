//! The save family (`docs:save`, `docs:save-as-pick`, `docs:save-as`,
//! `docs:save-new`, `docs:save-into-project`, `docs:save-to`), the crash
//! recovery copy (`docs:write-recovery`, `docs:write-recovery-now-done`),
//! the password intents and the small dialogs around a save.
//!
//! The bytes are prepared (`prepareDocSave`: pictures put back, the open
//! password applied) and written on a worker. The TypeScript re-checked the
//! tab's teardown after each await of that work; here the work of one
//! worker is one step, so the checks fall where the worker comes back.

use std::rc::Rc;

use jscompat::JsonValue;
use jscompat::node_path::host::{dirname, is_absolute, join};
use main_core::docs::atomic_write::atomic_write_file;
use main_core::docs::byte_handoff::MediaPermit;
use main_core::docs::doc_format::{
    DocFormat, PreparedSave, RecoveryBytesDeps, SaveBytesDeps, doc_format_of_path,
    prepare_recovery_bytes, prepare_save_bytes,
};
use main_core::docs::external_change::is_externally_modified;
use main_core::docs::save_as_formats::{
    SaveAsKind, save_as_default_path, save_as_filters, save_as_target,
};
use main_core::packages::electron_utils::dialog_memory::save_as_suggestion;
use main_core::packages::electron_utils::marked_odt::{
    MarkedOdtText, marked_odt_box, marked_odt_choice,
};
use shell_contract::hosts::MessageBox;
use shell_contract::ipc::Responder;
use shell_contract::{IpcValue, ViewId};

use super::adapters::picked_path;
use super::hosts::{DocsEngineHost, basename};
use super::owned_save::{self, Scope};
use super::owned_write::{self, Permission};
use super::passwords::Snapshot;
use super::recovery::{CopyResult, CopyTarget, write_copy};
use super::{
    Docs, Obj, array_buffer, error_text, failed, js_filters, js_obj, js_str, node_text, not_ok,
    sha256_hex, stat,
};
use crate::kit::{self, Entry, IntoIpc};
use main_core::packages::electron_utils::atomic_write::owned::{self, Admission};

enum SaveKind {
    InPlace,
    SaveAs { source: Option<String> },
    New,
    Explicit { existed: bool },
}

fn save_effect<T>(scope: &Option<Scope>, effect: impl FnOnce() -> T) -> Result<T, String> {
    match scope {
        Some(scope) => scope.effect(effect),
        None => Ok(effect()),
    }
}

fn save_pick(
    docs: &Docs,
    view: ViewId,
    current: DocFormat,
    exports: bool,
    result: JsonValue,
    scope: Option<Scope>,
) -> shell_contract::ipc::Reply {
    let Some(file_path) = picked_path(&result, "filePath") else {
        return Ok(IpcValue::Null);
    };
    if docs.is_torn_down(view) {
        return Ok(IpcValue::Null);
    }
    let (path, kind) = save_as_target(&file_path, current, exports);
    let picked = Obj::new()
        .set("path", path.clone())
        .set("kind", kind.as_str());
    match kind {
        SaveAsKind::Docx | SaveAsKind::Odt => {
            match scope {
                Some(scope) => scope.record_pick(path).map_err(kit::error)?,
                None => docs.save_as_picks.insert(view, path),
            }
            Ok(picked.done())
        }
        SaveAsKind::Png => {
            let dir = dirname(&path);
            let pdf_path = join(&[
                &docs.temp_dir(),
                &format!("seuoffice-docs-images-{}.pdf", docs.shell.random_uuid()),
            ]);
            save_effect(&scope, || docs.add_image_export(view, &pdf_path, &dir))
                .map_err(kit::error)?;
            Ok(picked.set("dir", dir).set("pdfPath", pdf_path).done())
        }
        _ => {
            save_effect(&scope, || docs.allow_pdf_write(view, &path)).map_err(kit::error)?;
            Ok(picked.done())
        }
    }
}

const NOT_OPENED: &str = "save target is not an opened document";

/// `MARKED_EXPORT_FORMATS`: the formats that ask before marked content is written
const MARKED_EXPORT_FORMATS: [&str; 6] = ["odt", "pdf", "hwp", "html", "md", "png"];

/// What a write left: what landed, the plain document, the absorbed picture
/// hashes, and the disk state to remember
pub(crate) struct Written {
    hash: String,
    plain: Vec<u8>,
    hashes: Vec<String>,
    encrypted: bool,
    disk: Option<(f64, f64)>,
}

/// `Buffer.from(data)` of a value that is no buffer
fn bad_data(v: &IpcValue) -> String {
    let received = match v {
        IpcValue::Undefined => "undefined".to_owned(),
        IpcValue::Null => "null".to_owned(),
        IpcValue::Bool(b) => format!("type boolean ({b})"),
        IpcValue::Number(n) => format!("type number ({})", jscompat::number_to_string(*n)),
        _ => "an instance of Object".to_owned(),
    };
    format!(
        "TypeError [ERR_INVALID_ARG_TYPE]: The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object. Received {received}"
    )
}

/// The bytes a save writes (`prepareDocSave` past its password snapshot);
/// `Err` is the thrown `Error`'s message
fn prepare(
    engine: &DocsEngineHost,
    format: DocFormat,
    data: &[u8],
    password: Option<&str>,
    permit: MediaPermit,
    cancel: shell_contract::runtime::CancelToken,
) -> Result<PreparedSave, String> {
    prepare_save_bytes(
        format,
        data,
        &mut SaveBytesDeps {
            materialize: &mut |b| {
                engine.materialize_lazy_docx(b.to_vec(), permit.clone(), cancel.clone())
            },
            encrypt: &mut |b, p| engine.encrypt_docx(b, p),
            password,
        },
    )
}

/// `atomicWriteFile` and the stat `rememberDiskState` takes after it
fn write(
    path: &str,
    prepared: PreparedSave,
    scope: Option<&Scope>,
    admission: Option<Admission>,
) -> Result<Written, String> {
    if scope.is_some_and(|scope| scope.target().is_some_and(|target| target != path)) {
        return Err("owned save target does not match the request".into());
    }
    match admission {
        Some(admission) => owned::write_file(path, &prepared.bytes, admission)
            .map_err(|error| node_text(&error))?,
        None if scope.is_some() => {
            return Err("owned save publication was not prepared on the document actor".into());
        }
        None => atomic_write_file(path, &prepared.bytes).map_err(|error| node_text(&error))?,
    }
    Ok(written(path, prepared))
}

fn written(path: &str, prepared: PreparedSave) -> Written {
    Written {
        hash: sha256_hex(&prepared.bytes),
        plain: prepared.plain,
        hashes: prepared.hashes,
        encrypted: prepared.encrypted,
        disk: stat(path).ok(),
    }
}

/// `uniquePathIn(dir, fileName)`
pub(crate) fn unique_path_in(dir: &str, file_name: &str) -> String {
    let dot = file_name.rfind('.').filter(|d| *d > 0);
    let base = dot.map_or(file_name, |d| &file_name[..d]);
    let ext = dot.map_or("", |d| &file_name[d..]);
    let mut candidate = join(&[dir, file_name]);
    let mut i = 2;
    while std::path::Path::new(&candidate).exists() {
        candidate = join(&[dir, &format!("{base}-{i}{ext}")]);
        i += 1;
    }
    candidate
}

/// `sanitizeAiDocFileBase(title)`
pub(crate) fn sanitize_file_base(title: &str) -> String {
    let replaced: String = title
        .chars()
        .map(|c| {
            if "/\\:*?\"<>|".contains(c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = jscompat::trim(&replaced);
    let cut: String =
        String::from_utf16_lossy(&trimmed.encode_utf16().take(80).collect::<Vec<_>>());
    let cleaned = jscompat::trim(&cut).to_owned();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "Untitled".into()
    } else {
        cleaned
    }
}

/// `text.slice(0, n)` in UTF-16 code units
fn slice_utf16(text: &str, n: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(n).collect::<Vec<_>>())
}

impl Docs {
    /// The password snapshot of `prepareDocSave` (docx only)
    fn snapshot_for(&self, view: ViewId, path: &str, source: Option<&str>) -> Option<Snapshot> {
        (doc_format_of_path(path).unwrap_or(DocFormat::Docx) == DocFormat::Docx)
            .then(|| self.passwords.borrow().snapshot(view, source))
    }

    /// `commitSavePassword(wcId, passwordState, filePath)`
    fn commit_password(&self, view: ViewId, snapshot: &Option<Snapshot>, path: &str) -> bool {
        let mut passwords = self.passwords.borrow_mut();
        match snapshot {
            Some(s) => passwords.commit_save(view, s, path),
            None => {
                passwords.remember(view, path, None);
                passwords.mark_encrypted(view, path, false);
                false
            }
        }
    }

    /// The bookkeeping every save does once its file landed: the disk state,
    /// the lazy pictures, the password; the answer's `passwordIntentPending`
    fn after_write(
        &self,
        view: ViewId,
        path: &str,
        w: &Written,
        snapshot: &Option<Snapshot>,
        permit: MediaPermit,
    ) -> Result<bool, String> {
        self.remember_disk(view, path, w.hash.clone(), w.disk);
        // The file already landed; retain its password even if the media actor failed.
        let pending = self.commit_password(view, snapshot, path);
        self.engine.point_lazy_media_at(
            &w.hashes,
            path,
            permit,
            w.encrypted.then(|| w.plain.clone()),
        )?;
        Ok(pending)
    }

    /// `{ ok: true, path?, passwordIntentPending, ...reissuedDoc(...) }`
    fn saved_reply(
        self: &Rc<Self>,
        path: Option<String>,
        pending: bool,
        w: Written,
        permit: MediaPermit,
        scope: Option<Scope>,
        responder: Responder,
    ) {
        let mut reply = Obj::new().set("ok", true);
        if let Some(path) = path {
            reply = reply.set("path", path);
        }
        reply = reply.set("passwordIntentPending", pending);
        if let Some(scope) = &scope {
            if let Err(error) = scope.check() {
                return responder.reply(Err(kit::error(error)));
            }
        }
        if !w.encrypted || w.hashes.is_empty() {
            return responder.reply(if permit.is_active() {
                Ok(reply.done())
            } else {
                Err(kit::error("The document media session is closed."))
            });
        }
        let engine = self.engine.clone();
        let worker_permit = permit.clone();
        kit::on_worker(
            &self.services,
            move || engine.hand_off_bytes(w.plain, worker_permit),
            move |result| {
                if !permit.is_active() {
                    return responder
                        .reply(Err(kit::error("The document media session is closed.")));
                }
                if let Some(scope) = &scope {
                    if let Err(error) = scope.check() {
                        return responder.reply(Err(kit::error(error)));
                    }
                }
                responder.reply(
                    result
                        .map(|url| reply.set("dataUrl", url).done())
                        .map_err(kit::error),
                );
            },
        );
    }

    fn note_saved(&self, view: ViewId, paths: &[&str]) {
        self.with_store(|store| self.book.borrow_mut().note_saved(store, view, paths));
    }

    fn complete_save(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        result: Result<Written, String>,
        snapshot: Option<Snapshot>,
        permit: MediaPermit,
        scope: Option<Scope>,
        kind: SaveKind,
        responder: Responder,
    ) {
        let w = match result {
            Ok(written) => written,
            Err(error) => return responder.reply(Ok(failed(error))),
        };
        if self.is_torn_down(view) || !permit.is_active() {
            if scope.is_none()
                && matches!(kind, SaveKind::New | SaveKind::Explicit { existed: false })
            {
                let _ = std::fs::remove_file(&path);
            }
            return responder.reply(Ok(if matches!(kind, SaveKind::InPlace) {
                failed(NOT_OPENED)
            } else {
                not_ok()
            }));
        }
        let pending = match save_effect(&scope, || {
            if matches!(kind, SaveKind::SaveAs { .. } | SaveKind::New) {
                self.allow_doc_write(view, &path);
                self.remember_open_doc(view, &path);
            }
            self.after_write(view, &path, &w, &snapshot, permit.clone())
        })
        .and_then(|result| result)
        {
            Ok(pending) => pending,
            Err(error) => return responder.reply(Err(kit::error(error))),
        };
        let mut stale_paths = vec![path.clone()];
        match &kind {
            SaveKind::SaveAs {
                source: Some(source),
            } => stale_paths.push(source.clone()),
            SaveKind::Explicit { .. } => {
                if let Some(previous) = self.open_doc.get(view).filter(|previous| *previous != path)
                {
                    stale_paths.push(previous);
                }
            }
            _ => {}
        }
        if let Some(owned) = scope.clone() {
            let docs = self.clone();
            let reply_path = path.clone();
            let in_place = matches!(kind, SaveKind::InPlace);
            let explicit = matches!(kind, SaveKind::Explicit { .. });
            return owned_save::after_write(
                self,
                view,
                path,
                stale_paths,
                owned,
                !in_place,
                explicit,
                Box::new(move |result| match result {
                    Err(error) => responder.reply(Err(kit::error(error))),
                    Ok(saved) => docs.saved_reply(
                        if in_place {
                            None
                        } else if explicit {
                            Some(reply_path)
                        } else {
                            Some(saved)
                        },
                        pending,
                        w,
                        permit,
                        scope,
                        responder,
                    ),
                }),
            );
        }
        self.note_saved(
            view,
            &stale_paths.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        self.push_recent(&path);
        let reply_path = match kind {
            SaveKind::InPlace => None,
            SaveKind::Explicit { .. } => {
                self.remember_open_doc(view, &path);
                self.notify_file_saved(view, &path);
                Some(path)
            }
            SaveKind::SaveAs { .. } | SaveKind::New => Some(self.notify_file_saved(view, &path)),
        };
        self.saved_reply(reply_path, pending, w, permit, None, responder);
    }
    /// `docs:save` past the external-modification check
    fn save_now(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        data: IpcValue,
        scope: Option<Scope>,
        cancel: shell_contract::runtime::CancelToken,
        responder: Responder,
    ) {
        if self.is_torn_down(view) || !self.can_doc_write(view, &path) {
            return responder.reply(Ok(failed(NOT_OPENED)));
        }
        if let Some(scope) = &scope {
            if let Err(error) = scope.capture_current_destination(self, &path) {
                return responder.reply(Err(kit::error(error)));
            }
        }
        let Some(permit) = self.media_permit(view) else {
            return responder.reply(Err(kit::error(
                "The document media session is unavailable.",
            )));
        };
        let worker_permit = permit.clone();
        let snapshot = self.snapshot_for(view, &path, Some(&path));
        let password = snapshot.as_ref().and_then(|s| s.password.clone());
        let engine = self.engine.clone();
        let target = path.clone();
        let docs = Rc::clone(self);
        let worker_scope = scope.clone();
        let admission = scope.as_ref().map(|scope| {
            owned_write::admission(self, scope.clone(), &path, Permission::ExistingDocument)
        });
        kit::on_worker(
            &self.services,
            move || {
                let bytes = kit::as_bytes(&data).ok_or_else(|| bad_data(&data))?;
                let format = doc_format_of_path(&target).unwrap_or(DocFormat::Docx);
                let prepared = prepare(
                    &engine,
                    format,
                    bytes,
                    password.as_deref(),
                    worker_permit,
                    cancel,
                )
                .map_err(|m| error_text(&m))?;
                write(&target, prepared, worker_scope.as_ref(), admission)
            },
            move |result| {
                docs.complete_save(
                    view,
                    path,
                    result,
                    snapshot,
                    permit,
                    scope,
                    SaveKind::InPlace,
                    responder,
                );
            },
        );
    }
}

pub(super) fn entries(docs: &Rc<Docs>) -> Vec<Entry> {
    let mut all = Vec::new();

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:save",
        move |cx, mut args, responder| {
            let view = cx.sender.view;
            let scope = match d.owned_save_scope(cx, args.get(3)) {
                Ok(scope) => scope,
                Err(error) => return responder.reply(Err(error)),
            };
            let Some(path) = args
                .str(0)
                .filter(|p| d.can_doc_write(view, p))
                .map(str::to_owned)
            else {
                return responder.reply(Ok(failed(NOT_OPENED)));
            };
            let auto = args.get(2) == &IpcValue::Bool(true);
            let data = args.take(1);
            let cancel = cx.cancel.clone();
            let recorded = d
                .disk_states
                .with(view, |s| s.and_then(|s| s.get(&path).cloned()));
            let source = path.clone();
            let d2 = d.clone();
            kit::on_worker(
                &d.services,
                move || {
                    let current = stat(&source).ok();
                    is_externally_modified(recorded.as_ref(), current, &mut || {
                        Ok(std::fs::read(&source).ok().map(|b| sha256_hex(&b)))
                    })
                    .unwrap_or(false)
                },
                move |changed| {
                    if changed {
                        let external = Obj::new()
                            .set("ok", false)
                            .set("reason", "external-modified")
                            .done();
                        // autosave never clobbers another program's edits silently
                        if auto {
                            return responder.reply(Ok(external));
                        }
                        let options = MessageBox {
                            kind: Some("warning".into()),
                            message: d2.tm("extModifiedMsg"),
                            detail: Some(d2.tm("extModifiedDetail")),
                            buttons: vec![d2.tm("btnOverwrite"), d2.tm("btnCancel")],
                            default_id: Some(0),
                            cancel_id: Some(1),
                            ..Default::default()
                        };
                        if let Some(owned) = scope.clone() {
                            let resumed = d2.clone();
                            return owned_save::message(
                                &d2,
                                owned,
                                options,
                                Box::new(move |answer| match answer {
                                    Err(error) => responder.reply(Err(kit::error(error))),
                                    Ok(0) => {
                                        resumed.save_now(view, path, data, scope, cancel, responder)
                                    }
                                    Ok(_) => responder.reply(Ok(external)),
                                }),
                            );
                        }
                        let response = d2.message(d2.parent(view), options);
                        if response != 0 {
                            return responder.reply(Ok(external));
                        }
                    }
                    d2.save_now(view, path, data, scope, cancel, responder)
                },
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:save-as-pick",
        move |cx, args, responder| {
            let view = cx.sender.view;
            let scope = match d.owned_save_scope(cx, args.get(4)) {
                Ok(scope) => scope,
                Err(error) => return responder.reply(Err(error)),
            };
            if scope.as_ref().is_some_and(|scope| scope.target().is_some()) {
                return responder.reply(Err(kit::error(
                    "an explicit save cannot choose another destination",
                )));
            }
            if d.is_torn_down(view) {
                return responder.reply(Ok(IpcValue::Null));
            }
            let Some(default_name) = args.str(0) else {
                return responder.reply(Ok(IpcValue::Null));
            };
            let current = if args.str(2) == Some("odt") {
                DocFormat::Odt
            } else {
                DocFormat::Docx
            };
            let exports = args.get(3) == &IpcValue::Bool(true);
            let source = args.str(1);
            let project_dir =
                if source.is_some_and(|source| !source.is_empty()) || d.open_doc.contains(view) {
                    None
                } else {
                    d.shell.new_document_dir(view)
                };
            let suggestion = match project_dir.filter(|dir| !dir.is_empty()) {
                Some(dir) if basename(default_name) == default_name => join(&[&dir, default_name]),
                _ => save_as_suggestion(source, default_name),
            };
            let filters: Vec<(String, Vec<&str>)> =
                save_as_filters(current, exports, &mut |key| d.tm(key))
                    .into_iter()
                    .map(|filter| (filter.name, filter.extensions))
                    .collect();
            let options = js_obj(vec![
                ("title", js_str(d.tm("dlgSaveAs"))),
                (
                    "defaultPath",
                    js_str(save_as_default_path(&suggestion, exports)),
                ),
                ("filters", js_filters(&filters)),
            ]);
            if let Some(owned) = scope.clone() {
                let docs = d.clone();
                owned_save::save_dialog(
                    &d,
                    owned,
                    options,
                    Some((current, exports)),
                    Box::new(move |result| {
                        responder.reply(result.and_then(|result| {
                            save_pick(&docs, view, current, exports, result, scope)
                        }));
                    }),
                );
            } else {
                let result = d.save_dialog(view, options);
                responder.reply(save_pick(&d, view, current, exports, result, None));
            }
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:save-as",
        move |cx, mut args, responder| {
            let view = cx.sender.view;
            let scope = match d.owned_save_scope(cx, args.get(3)) {
                Ok(scope) => scope,
                Err(error) => return responder.reply(Err(error)),
            };
            if d.is_torn_down(view) {
                return responder.reply(Ok(not_ok()));
            }
            let Some(permit) = d.media_permit(view) else {
                return responder.reply(Err(kit::error(
                    "The document media session is unavailable.",
                )));
            };
            let worker_permit = permit.clone();
            let Some(path) = args.str(0).map(str::to_owned) else {
                return responder.reply(Ok(failed("save target was not picked")));
            };
            let picked = match &scope {
                Some(scope) => match scope.take_pick(&path) {
                    Ok(picked) => picked,
                    Err(error) => return responder.reply(Err(kit::error(error))),
                },
                None => {
                    let picked = d.save_as_picks.get(view).as_ref() == Some(&path);
                    if picked {
                        d.save_as_picks.remove(view);
                    }
                    picked
                }
            };
            if !picked {
                return responder.reply(Ok(failed("save target was not picked")));
            }
            let source = args.str(2).filter(|s| !s.is_empty()).map(str::to_owned);
            let snapshot = d.snapshot_for(view, &path, source.as_deref());
            let password = snapshot.as_ref().and_then(|s| s.password.clone());
            let data = args.take(1);
            let engine = d.engine.clone();
            let target = path.clone();
            let d2 = d.clone();
            let cancel = cx.cancel.clone();
            kit::on_worker(
                &d.services,
                move || {
                    let bytes = kit::as_bytes(&data).ok_or_else(|| bad_data(&data))?;
                    let format = doc_format_of_path(&target).unwrap_or(DocFormat::Docx);
                    prepare(
                        &engine,
                        format,
                        bytes,
                        password.as_deref(),
                        worker_permit,
                        cancel,
                    )
                    .map_err(|m| error_text(&m))
                },
                move |prepared| {
                    let prepared = match prepared {
                        Err(e) => return responder.reply(Ok(failed(e))),
                        Ok(p) => p,
                    };
                    // the tab may have closed meanwhile: Save As may overwrite, so check first
                    if d2.is_torn_down(view) || !permit.is_active() {
                        return responder.reply(Ok(not_ok()));
                    }
                    let target = path.clone();
                    let d3 = d2.clone();
                    let worker_scope = scope.clone();
                    let admission = scope.as_ref().map(|scope| {
                        owned_write::admission(&d2, scope.clone(), &path, Permission::RequestTarget)
                    });
                    kit::on_worker(
                        &d2.services,
                        move || write(&target, prepared, worker_scope.as_ref(), admission),
                        move |result| {
                            d3.complete_save(
                                view,
                                path,
                                result,
                                snapshot,
                                permit,
                                scope,
                                SaveKind::SaveAs { source },
                                responder,
                            );
                        },
                    );
                },
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:save-new",
        move |cx, mut args, responder| {
            let view = cx.sender.view;
            let scope = match d.owned_save_scope(cx, args.get(2)) {
                Ok(scope) => scope,
                Err(error) => return responder.reply(Err(error)),
            };
            if d.is_torn_down(view) {
                return responder.reply(Ok(not_ok()));
            }
            let Some(permit) = d.media_permit(view) else {
                return responder.reply(Err(kit::error(
                    "The document media session is unavailable.",
                )));
            };
            let worker_permit = permit.clone();
            let Some(name) = args.str(0).map(str::to_owned) else {
                return responder.reply(Ok(failed(
                    "TypeError: fileName.lastIndexOf is not a function",
                )));
            };
            let dir = match d.default_save_dir() {
                Ok(dir) => dir,
                Err(e) => return responder.reply(Ok(failed(error_text(&e)))),
            };
            let path = unique_path_in(&dir, &name);
            let snapshot = d.snapshot_for(view, &path, None);
            let password = snapshot.as_ref().and_then(|s| s.password.clone());
            let data = args.take(1);
            let engine = d.engine.clone();
            let target = path.clone();
            let d2 = d.clone();
            let worker_scope = scope.clone();
            let admission = scope.as_ref().map(|scope| {
                owned_write::admission(&d, scope.clone(), &path, Permission::RequestTarget)
            });
            let cancel = cx.cancel.clone();
            kit::on_worker(
                &d.services,
                move || {
                    let bytes = kit::as_bytes(&data).ok_or_else(|| bad_data(&data))?;
                    let format = doc_format_of_path(&target).unwrap_or(DocFormat::Docx);
                    let prepared = prepare(
                        &engine,
                        format,
                        bytes,
                        password.as_deref(),
                        worker_permit,
                        cancel,
                    )
                    .map_err(|m| error_text(&m))?;
                    write(&target, prepared, worker_scope.as_ref(), admission)
                },
                move |result| {
                    d2.complete_save(
                        view,
                        path,
                        result,
                        snapshot,
                        permit,
                        scope,
                        SaveKind::New,
                        responder,
                    );
                },
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:save-into-project",
        move |cx, mut args, responder| {
            let view = cx.sender.view;
            let scope = match d.owned_save_scope(cx, args.get(3)) {
                Ok(scope) => scope,
                Err(error) => return responder.reply(Err(error)),
            };
            if scope.as_ref().is_some_and(|scope| scope.target().is_some()) {
                return responder.reply(Err(kit::error(
                    "an explicit save cannot choose a project destination",
                )));
            }
            let skipped = || Obj::new().set("ok", false).set("skipped", true).done();
            // an orphaned renderer, or a document that has its file already
            if !d.shell.writes_new_documents() || d.is_torn_down(view) || d.open_doc.contains(view)
            {
                return responder.reply(Ok(skipped()));
            }
            let Some(permit) = d.media_permit(view) else {
                return responder.reply(Err(kit::error(
                    "The document media session is unavailable.",
                )));
            };
            let worker_permit = permit.clone();
            let (Some(default_name), Some(_)) =
                (args.str(0).map(str::to_owned), array_buffer(args.get(2)))
            else {
                return responder.reply(Ok(failed("bad save request")));
            };
            let extension = if args.str(1) == Some("odt") {
                ".odt"
            } else {
                ".docx"
            };
            let named = jscompat::js_regex!(r"\.(docx|odt)$", "i")
                .replace_all(&basename(&default_name.replace('\\', "/")), |_| {
                    String::new()
                });
            let file_name = format!("{}{extension}", sanitize_file_base(&named));
            let snapshot = d.snapshot_for(view, &file_name, None);
            let password = snapshot.as_ref().and_then(|s| s.password.clone());
            let data = args.take(2);
            let engine = d.engine.clone();
            let name = file_name.clone();
            let d2 = d.clone();
            let cancel = cx.cancel.clone();
            kit::on_worker(
                &d.services,
                move || {
                    let format = doc_format_of_path(&name).unwrap_or(DocFormat::Docx);
                    prepare(
                        &engine,
                        format,
                        kit::as_bytes(&data).unwrap_or_default(),
                        password.as_deref(),
                        worker_permit,
                        cancel,
                    )
                },
                move |prepared| {
                    let prepared = match prepared {
                        Err(e) => return responder.reply(Ok(failed(e))),
                        Ok(p) => p,
                    };
                    if d2.is_torn_down(view) || !permit.is_active() {
                        return responder.reply(Ok(not_ok()));
                    }
                    let shell = d2.shell.clone();
                    let d3 = d2.clone();
                    let worker_scope = scope.clone();
                    let publication = scope.as_ref().map(|scope| {
                        owned_write::admission(
                            &d2,
                            scope.clone(),
                            &file_name,
                            Permission::ProjectDocument,
                        )
                    });
                    kit::on_worker(
                        &d2.services,
                        move || {
                            let placed = match &worker_scope {
                                Some(scope) => {
                                    let publication = publication.ok_or_else(|| {
                                        "the project save publication was not captured".to_owned()
                                    });
                                    publication.and_then(|publication| {
                                        shell.write_new_document_owned(
                                            scope.origin(),
                                            scope.destination(),
                                            scope.cancel(),
                                            scope.pending(),
                                            scope.effects(),
                                            publication,
                                            &file_name,
                                            &prepared.bytes,
                                        )
                                    })
                                }
                                None => shell.write_new_document(
                                    view,
                                    &file_name,
                                    prepared.bytes.clone(),
                                )?,
                            };
                            Some(placed.map(|p| p.map(|path| (written(&path, prepared), path))))
                        },
                        move |placed| {
                            let (w, path) = match placed {
                                None | Some(Ok(None)) => return responder.reply(Ok(skipped())),
                                Some(Err(e)) => return responder.reply(Ok(failed(e))),
                                Some(Ok(Some(p))) => p,
                            };
                            d3.complete_save(
                                view,
                                path,
                                Ok(w),
                                snapshot,
                                permit,
                                scope,
                                SaveKind::New,
                                responder,
                            );
                        },
                    );
                },
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:save-to",
        move |cx, mut args, responder| {
            let view = cx.sender.view;
            let scope = match d.owned_save_scope(cx, args.get(3)) {
                Ok(scope) => scope,
                Err(error) => return responder.reply(Err(error)),
            };
            if d.is_torn_down(view) {
                return responder.reply(Ok(not_ok()));
            }
            let Some(permit) = d.media_permit(view) else {
                return responder.reply(Err(kit::error(
                    "The document media session is unavailable.",
                )));
            };
            let worker_permit = permit.clone();
            let Some(path) = args.str(0).filter(|p| is_absolute(p)).map(str::to_owned) else {
                return responder.reply(Ok(failed("path must be absolute")));
            };
            if doc_format_of_path(&path).is_none() {
                return responder.reply(Ok(failed("path must point to a .docx or .odt file")));
            }
            let authorized = match scope.as_ref().and_then(Scope::target) {
                Some(target) => target == path,
                None => d.can_doc_write(view, &path),
            };
            if !authorized {
                return responder.reply(Ok(failed("save target was not authorized")));
            }
            if let Some(scope) = &scope {
                if scope.target().is_none() {
                    if let Err(error) = scope.capture_current_destination(&d, &path) {
                        return responder.reply(Err(kit::error(error)));
                    }
                }
            }
            let existed = std::path::Path::new(&path).exists();
            if !kit::truthy(args.get(2)) && existed {
                return responder.reply(Ok(failed(format!(
                    "file already exists: {path} (pass overwrite:true to replace it)"
                ))));
            }
            let snapshot = d.snapshot_for(view, &path, None);
            let password = snapshot.as_ref().and_then(|s| s.password.clone());
            let data = args.take(1);
            let engine = d.engine.clone();
            let target = path.clone();
            let d2 = d.clone();
            let worker_scope = scope.clone();
            let admission = scope.as_ref().map(|scope| {
                let permission = if scope.target().is_some() {
                    Permission::RequestTarget
                } else {
                    Permission::ExistingDocument
                };
                owned_write::admission(&d, scope.clone(), &path, permission)
            });
            let cancel = cx.cancel.clone();
            kit::on_worker(
                &d.services,
                move || {
                    let dir = dirname(&target);
                    match &admission {
                        Some(admission) => owned::create_parent(&target, admission)
                            .map_err(|error| node_text(&error))?,
                        None => std::fs::create_dir_all(&dir)
                            .map_err(|e| node_text(&super::fs_error(&e, "mkdir", &dir)))?,
                    }
                    let bytes = kit::as_bytes(&data).ok_or_else(|| bad_data(&data))?;
                    let format = doc_format_of_path(&target).unwrap_or(DocFormat::Docx);
                    let prepared = prepare(
                        &engine,
                        format,
                        bytes,
                        password.as_deref(),
                        worker_permit,
                        cancel,
                    )
                    .map_err(|m| error_text(&m))?;
                    write(&target, prepared, worker_scope.as_ref(), admission)
                },
                move |result| {
                    d2.complete_save(
                        view,
                        path,
                        result,
                        snapshot,
                        permit,
                        scope,
                        SaveKind::Explicit { existed },
                        responder,
                    );
                },
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:write-recovery",
        move |cx, mut args, responder| {
            let view = cx.sender.view;
            if d.is_torn_down(view) || array_buffer(args.get(1)).is_none() {
                return responder.reply(Ok(not_ok()));
            }
            let untitled = args.get(0) == &IpcValue::Null;
            let file_path = args.str(0).map(str::to_owned);
            if !untitled
                && !file_path
                    .as_deref()
                    .is_some_and(|p| d.can_doc_write(view, p))
            {
                return responder.reply(Ok(not_ok()));
            }
            let meta = args.get(2).clone();
            let format = if untitled {
                if kit::field(&meta, "format") == &IpcValue::String("odt".into()) {
                    DocFormat::Odt
                } else {
                    DocFormat::Docx
                }
            } else {
                file_path
                    .as_deref()
                    .and_then(doc_format_of_path)
                    .unwrap_or(DocFormat::Docx)
            };
            let title = kit::as_str(kit::field(&meta, "title"))
                .map_or(String::new(), |t| slice_utf16(t, 260));
            let original_path = kit::as_str(kit::field(&meta, "originalPath"))
                .filter(|p| untitled && !p.is_empty())
                .map(str::to_owned);
            // a headless export's hidden window keeps no copy
            if d.shell.is_headless() {
                return responder.reply(Ok(not_ok()));
            }
            let target = match (&file_path, format) {
                (Some(p), _) if !untitled => CopyTarget::File(p.clone()),
                (_, DocFormat::Odt) => CopyTarget::Untitled(".odt"),
                _ => CopyTarget::Untitled(".docx"),
            };
            let pending = match d.with_store(|store| {
                d.book
                    .borrow_mut()
                    .begin_copy(store, view, &target, &|| d.shell.random_uuid())
            }) {
                Ok(p) => p,
                Err(_) => {
                    return responder
                        .reply(Ok(Obj::new().set("ok", false).set("failed", true).done()));
                }
            };
            // the copy follows the disk's password, never the desired next one: an
            // encrypted file without its password, or a new document waiting for
            // its first password save, skips the tick
            let protect: Result<Option<String>, ()> = match &file_path {
                _ if untitled => {
                    if d.passwords.borrow().snapshot(view, None).password.is_none() {
                        Ok(None)
                    } else {
                        Err(())
                    }
                }
                Some(p) if d.passwords.borrow().is_encrypted(view, p) => d
                    .passwords
                    .borrow()
                    .password_for(view, Some(p))
                    .map(Some)
                    .ok_or(()),
                _ => Ok(None),
            };
            let copy_title = if untitled {
                title
            } else {
                basename(file_path.as_deref().unwrap_or(""))
            };
            let data = args.take(1);
            let engine = d.engine.clone();
            let shared = d.recovery_shared.clone();
            let shell = d.shell.clone();
            let user_data = d.user_data(&[]);
            let original = original_path.clone();
            let d2 = d.clone();
            let Some(permit) = d.media_permit(view) else {
                return responder.reply(Err(kit::error(
                    "The document media session is unavailable.",
                )));
            };
            let cancel = cx.cancel.clone();
            kit::on_worker(
                &d.services,
                move || {
                    let bytes = prepare_recovery_bytes(
                        format,
                        kit::as_bytes(&data).unwrap_or_default(),
                        &mut RecoveryBytesDeps {
                            materialize: &mut |b| {
                                engine.materialize_lazy_docx(
                                    b.to_vec(),
                                    permit.clone(),
                                    cancel.clone(),
                                )
                            },
                            protect: &mut |plain| match &protect {
                                Err(()) => Ok(None),
                                Ok(None) => Ok(Some(plain.to_vec())),
                                Ok(Some(pw)) => engine.encrypt_docx(plain, pw).map(Some),
                            },
                        },
                    )
                    .map_err(|_| ())?;
                    write_copy(
                        &shared, &user_data, &shell, &pending, bytes, copy_title, original,
                    )
                    .map(|r| (r, pending))
                    .map_err(|_| ())
                },
                move |result| {
                    responder.reply(Ok(match result {
                        Err(()) => Obj::new().set("ok", false).set("failed", true).done(),
                        Ok((CopyResult::Skipped, _)) => not_ok(),
                        Ok((CopyResult::Written, pending)) => {
                            d2.with_store(|store| {
                                d2.book.borrow_mut().copy_written(
                                    store,
                                    view,
                                    &pending.identity,
                                    original_path.as_deref(),
                                )
                            });
                            Obj::new().set("ok", true).done()
                        }
                    }))
                },
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::send(
        "docs:write-recovery-now-done",
        move |cx, args| {
            if let (Some(token), Some(generation)) = (
                args.str(0),
                cx.native.as_ref().and_then(|native| native.renderer()),
            ) {
                let result = args.str(1).map_or(Ok(()), |message| {
                    Err(main_core::shell::renderer_disposal::Failure(
                        message.to_owned(),
                    ))
                });
                d.close
                    .recovery_now_done(cx.sender.view, generation, token, result);
            }
            Ok(())
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke("docs:set-password", move |cx, args| {
        let view = cx.sender.view;
        if d.is_torn_down(view) {
            return Ok(not_ok());
        }
        let path = match args.get(0) {
            IpcValue::Null => None,
            IpcValue::String(p) => Some(p.as_str()),
            _ => return Ok(not_ok()),
        };
        let password = match args.get(1) {
            IpcValue::Null => None,
            IpcValue::String(p) if !p.is_empty() => Some(p.as_str()),
            _ => return Ok(not_ok()),
        };
        // only the document this renderer has open (the grant of a save)
        if let Some(p) = path.filter(|p| !p.is_empty())
            && !d.can_doc_write(view, p)
        {
            return Ok(not_ok());
        }
        d.passwords.borrow_mut().set(view, path, password);
        Ok(Obj::new().set("ok", true).done())
    }));

    let d = docs.clone();
    all.push(kit::invoke(
        "docs:password-intent-revision",
        move |cx, _args| {
            if d.is_torn_down(cx.sender.view) {
                return Ok((-1.0).into_ipc());
            }
            Ok((d.passwords.borrow().current_revision() as f64).into_ipc())
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke(
        "docs:discard-password-intents",
        move |cx, args| {
            let view = cx.sender.view;
            if d.is_torn_down(view) {
                return Ok(not_ok());
            }
            let Some(through) = args
                .num(0)
                .filter(|n| n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 && *n >= 0.0)
            else {
                return Ok(not_ok());
            };
            d.passwords.borrow_mut().discard(view, through as u64);
            Ok(Obj::new().set("ok", true).done())
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:confirm-marked-export",
        move |cx, args, responder| {
            let view = cx.sender.view;
            let scope = match d.owned_save_scope(cx, args.get(1)) {
                Ok(scope) => scope,
                Err(error) => return responder.reply(Err(error)),
            };
            if d.is_torn_down(view) {
                return responder.reply(Ok("cancel".into_ipc()));
            }
            // an unknown format answers as a cancel
            let Some(format) = args.str(0).filter(|f| MARKED_EXPORT_FORMATS.contains(f)) else {
                return responder.reply(Ok("cancel".into_ipc()));
            };
            let odt = format == "odt";
            let b = marked_odt_box(&MarkedOdtText {
                title: d.tm("markedTitle"),
                message: d.tm(if odt {
                    "markedOdtMessage"
                } else {
                    "markedExportMessage"
                }),
                detail: d.tm(if odt {
                    "markedOdtDetail"
                } else {
                    "markedExportDetail"
                }),
                save_docx: Some(d.tm("markedDocx")),
                clear_marks: d.tm(if odt {
                    "markedOdtClear"
                } else {
                    "markedExportClear"
                }),
                cancel: d.tm("btnCancel"),
            });
            let options = MessageBox {
                    kind: Some(b.kind.into()),
                    title: Some(b.title),
                    message: b.message,
                    detail: Some(b.detail),
                    buttons: b.buttons,
                    default_id: Some(b.default_id as usize),
                    cancel_id: Some(b.cancel_id),
                    checkbox_label: None,
            };
            let complete = move |response: Result<usize, String>| {
                responder.reply(response.map(|response| {
                    match marked_odt_choice(response as f64, true) {
                        main_core::packages::electron_utils::marked_odt::MarkedOdtChoice::Docx => "docx",
                        main_core::packages::electron_utils::marked_odt::MarkedOdtChoice::Clear => "clear",
                        main_core::packages::electron_utils::marked_odt::MarkedOdtChoice::Cancel => "cancel",
                    }.into_ipc()
                }).map_err(kit::error));
            };
            match scope {
                Some(scope) => owned_save::message(&d, scope, options, Box::new(complete)),
                None => complete(Ok(d.message(d.parent(view), options))),
            }
        },
    ));

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:materialize-docx",
        move |cx, mut args, responder| {
            if d.is_torn_down(cx.sender.view) || array_buffer(args.get(0)).is_none() {
                return responder.reply(Ok(IpcValue::Null));
            }
            let Some(permit) = d.media_permit(cx.sender.view) else {
                return responder.reply(Err(kit::error(
                    "The document media session is unavailable.",
                )));
            };
            let worker_permit = permit.clone();
            let data = args.take(0);
            let engine = d.engine.clone();
            let cancel = cx.cancel.clone();
            kit::on_worker(
                &d.services,
                move || {
                    let bytes = kit::as_bytes(&data).unwrap_or_default().to_vec();
                    match engine.materialize_lazy_docx(bytes, worker_permit.clone(), cancel.clone())
                    {
                        Ok(materialized) => engine
                            .hand_off_bytes(materialized.bytes, worker_permit)
                            .map(Some),
                        Err(error) if cancel.is_cancelled() => Err(error),
                        Err(_) => Ok(None),
                    }
                },
                move |url| {
                    if !permit.is_active() {
                        return responder
                            .reply(Err(kit::error("The document media session is closed.")));
                    }
                    responder.reply(match url {
                        Ok(Some(url)) => Ok(Obj::new().set("dataUrl", url).done()),
                        Ok(None) => Ok(IpcValue::Null),
                        Err(error) => Err(kit::error(error)),
                    })
                },
            );
        },
    ));
    all
}
