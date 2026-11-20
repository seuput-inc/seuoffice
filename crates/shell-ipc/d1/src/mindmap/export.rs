//! `export-main.ts`: where an export goes and what is written. The renderer
//! draws the map. Save As copies carry a native request correlation; ordinary
//! exports ask where to put the file.

use jscompat::node_path::host::extname;
use jscompat::{JsObject, JsonValue, js_regex, trim};
use main_core::mindmap::atomic_write::atomic_write_file;
use main_core::mindmap::save_as_formats::MapSaveAsKind;
use main_core::packages::electron_utils::dialog_memory::{FallbackDir, save_as_suggestion};
use shell_contract::ViewId;
use shell_contract::ipc::{CallContext, Responder};
use shell_contract::{IpcError, IpcValue};

use super::app::MindmapApp;
use super::handlers::pick_save_path;
use super::util::{slice, utf16_len};
use crate::route::{self, Route};
use crate::value::{Args, as_bytes, as_str, field, obj, string, thrown};

/// A page larger than this is no map page this editor wrote
const MAX_PAGE_CHARS: usize = 200 * 1024 * 1024;
/// An outline larger than this is no outline of a map that opened
const MAX_OUTLINE_CHARS: usize = 64 * 1024 * 1024;
/// A picture larger than this is no picture of one sheet this editor drew
const MAX_PNG_BYTES: usize = 512 * 1024 * 1024;
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
/// What an SVG file written here starts with
const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

/// `EXPORT_KINDS`: the extension (with its dot) and the filter name of each kind
fn export_kind(kind: MapSaveAsKind) -> (&'static str, &'static str) {
    match kind {
        MapSaveAsKind::Png => (".png", "PNG"),
        MapSaveAsKind::Svg => (".svg", "SVG"),
        MapSaveAsKind::Pdf => (".pdf", "PDF"),
        MapSaveAsKind::Markdown => (".md", "Markdown"),
        MapSaveAsKind::Xmind => unreachable!("a map is saved, not exported"),
    }
}

/// `exportFileBase`: a suggested name as a file name: no path characters,
/// not too long, never empty
fn export_file_base(app: &MindmapApp, name: &IpcValue) -> String {
    let text = match name {
        IpcValue::String(text) => text.as_str(),
        _ => "",
    };
    let cleaned: String = text
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                || (c as u32) < 0x20
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let base = trim(&slice(&cleaned, 0, 80)).to_owned();
    // a name of dots alone is no name on Windows
    let base = js_regex!(r"^\.+$").replace_all(&base, |_| String::new());
    if base.is_empty() {
        app.tm("untitledFile")
    } else {
        base
    }
}

/// `withExportExtension`
fn with_export_extension(path: String, kind: MapSaveAsKind) -> String {
    let (extension, _) = export_kind(kind);
    if extname(&path).to_lowercase() == extension {
        path
    } else {
        format!("{path}{extension}")
    }
}

/// `pickTarget`: where the user wants the export, `None` when the dialog was cancelled
fn pick_target(
    app: &MindmapApp,
    view: ViewId,
    kind: MapSaveAsKind,
    suggested_name: &IpcValue,
) -> Result<Option<String>, String> {
    let (extension, filter) = export_kind(kind);
    let file_path = app.file_path(view);
    let default_path = save_as_suggestion(
        file_path.as_deref(),
        &format!("{}{extension}", export_file_base(app, suggested_name)),
    );
    let mut options = JsObject::new();
    options.set("defaultPath", JsonValue::String(default_path));
    let mut item = JsObject::new();
    item.set("name", JsonValue::String(filter.to_owned()));
    item.set(
        "extensions",
        JsonValue::Array(vec![JsonValue::String(extension[1..].to_owned())]),
    );
    options.set("filters", JsonValue::Array(vec![JsonValue::Object(item)]));
    let fallback = app.default_save_dir()?;
    Ok(
        pick_save_path(app, view, &options, Some(&FallbackDir::Dir(&fallback)))
            .map(|picked| with_export_extension(picked, kind)),
    )
}

/// Ordinary exports choose a path in their own dialog. Save As paths are
/// consumed only by the captured native request in `owned_export`.
fn target_of(
    app: &MindmapApp,
    view: ViewId,
    kind: MapSaveAsKind,
    request: &IpcValue,
) -> Result<Option<String>, String> {
    let given = field(request, "path");
    if matches!(given, IpcValue::Undefined) {
        return pick_target(app, view, kind, field(request, "suggestedName"));
    }
    Err("mindmap: a Save As export requires its native save request identifier".into())
}

fn export_failure(message: impl Into<String>) -> IpcValue {
    obj([("ok", IpcValue::Bool(false)), ("error", string(message))])
}

fn export_canceled() -> IpcValue {
    obj([
        ("ok", IpcValue::Bool(true)),
        ("canceled", IpcValue::Bool(true)),
    ])
}

fn export_written(path: String) -> IpcValue {
    obj([("ok", IpcValue::Bool(true)), ("path", string(path))])
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.len() > PNG_SIGNATURE.len() && bytes.starts_with(&PNG_SIGNATURE)
}

/// `isMapSvg`: one SVG element from start to end, and no script anywhere
fn is_map_svg(text: &IpcValue) -> Option<&str> {
    let IpcValue::String(text) = text else {
        return None;
    };
    (utf16_len(text) <= MAX_PAGE_CHARS
        && text.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\"")
        && text.ends_with("</svg>")
        && !js_regex!(r"<script\b", "i").test(text))
    .then_some(text.as_str())
}

/// the whole of a handler whose write is small: target, write, answer
fn write_export(
    app: &MindmapApp,
    view: ViewId,
    kind: MapSaveAsKind,
    request: &IpcValue,
    bytes: &[u8],
) -> IpcValue {
    match target_of(app, view, kind, request) {
        Err(message) => export_failure(message),
        Ok(None) => export_canceled(),
        Ok(Some(target)) => match atomic_write_file(&target, bytes) {
            Ok(()) => export_written(target),
            Err(error) => export_failure(error.message),
        },
    }
}

fn export_pdf(app: &MindmapApp, cx: &CallContext, args: Args, responder: Responder) {
    let view = cx.sender.view;
    let request = args.get(0);
    let html = match field(request, "html") {
        IpcValue::String(html) if !html.is_empty() && utf16_len(html) <= MAX_PAGE_CHARS => html,
        _ => {
            return responder.reply(Ok(export_failure("mindmap: bad PDF export request")));
        }
    };
    if as_str(field(request, "ownedSaveRequestId")).is_some() {
        let renderer = app.0.renderer.clone();
        let html = html.clone();
        return export_owned(
            app,
            cx,
            request,
            MapSaveAsKind::Pdf,
            Box::new(move |request| match &request.authority {
                crate::owned_save::SaveAuthority::Renderer(origin) => renderer.page_to_pdf_owned(
                    &html,
                    origin,
                    &request.destination,
                    &request.cancel,
                    &request.pending,
                    &request.participants(&[]),
                    &request.additional,
                ),
                crate::owned_save::SaveAuthority::Native(binding) => renderer.page_to_pdf_native(
                    &html,
                    binding,
                    &request.participants(&[]),
                    &request.cancel,
                    &request.pending,
                    &request.additional,
                ),
            }),
            responder,
        );
    }
    let target = match target_of(app, view, MapSaveAsKind::Pdf, request) {
        Err(message) => return responder.reply(Ok(export_failure(message))),
        Ok(None) => return responder.reply(Ok(export_canceled())),
        Ok(Some(target)) => target,
    };
    let renderer = app.0.renderer.clone();
    let html = html.clone();
    app.offload(
        move || -> Result<String, String> {
            let pdf = renderer.page_to_pdf(&html)?;
            atomic_write_file(&target, &pdf).map_err(|error| error.message)?;
            Ok(target)
        },
        move |_app, outcome| {
            responder.reply(Ok(match outcome {
                Ok(path) => export_written(path),
                Err(message) => export_failure(message),
            }))
        },
    );
}

pub(super) fn routes(app: &MindmapApp) -> Vec<Route> {
    let mut routes = Vec::new();
    for (channel, kind) in [
        ("mindmap:export-png", MapSaveAsKind::Png),
        ("mindmap:export-svg", MapSaveAsKind::Svg),
        ("mindmap:export-markdown", MapSaveAsKind::Markdown),
    ] {
        let a = app.clone();
        routes.push(route::invoke_later(channel, move |cx, args, responder| {
            let request = args.get(0);
            let bytes = match kind {
                MapSaveAsKind::Png => match as_bytes(field(request, "bytes")) {
                    Some(bytes) if is_png(bytes) && bytes.len() <= MAX_PNG_BYTES => bytes.to_vec(),
                    _ => {
                        return responder
                            .reply(Ok(export_failure("mindmap: bad picture export request")));
                    }
                },
                MapSaveAsKind::Svg => match is_map_svg(field(request, "svg")) {
                    Some(svg) => format!("{XML_DECLARATION}{svg}").into_bytes(),
                    None => {
                        return responder
                            .reply(Ok(export_failure("mindmap: bad SVG export request")));
                    }
                },
                MapSaveAsKind::Markdown => match field(request, "text") {
                    IpcValue::String(text) if utf16_len(text) <= MAX_OUTLINE_CHARS => {
                        text.as_bytes().to_vec()
                    }
                    _ => {
                        return responder
                            .reply(Ok(export_failure("mindmap: bad outline export request")));
                    }
                },
                _ => return responder.reply(Err(thrown("The map export kind is invalid"))),
            };
            if as_str(field(request, "ownedSaveRequestId")).is_some() {
                export_owned(
                    &a,
                    cx,
                    request,
                    kind,
                    Box::new(move |_| Ok(bytes)),
                    responder,
                );
            } else {
                responder.reply(Ok(write_export(&a, cx.sender.view, kind, request, &bytes)));
            }
        }));
    }
    let a = app.clone();
    routes.push(route::invoke_later(
        "mindmap:export-pdf",
        move |cx, args, responder| export_pdf(&a, cx, args, responder),
    ));
    routes
}

#[allow(clippy::type_complexity)]
fn export_owned(
    app: &MindmapApp,
    cx: &CallContext,
    value: &IpcValue,
    kind: MapSaveAsKind,
    produce: Box<dyn FnOnce(&crate::owned_save::OwnedSave) -> Result<Vec<u8>, String> + Send>,
    responder: Responder,
) {
    let prepared = (|| {
        let id = super::owned_save::request_id(
            as_str(field(value, "ownedSaveRequestId"))
                .ok_or_else(|| thrown("The export has no save request"))?,
        )?;
        let path = as_str(field(value, "path"))
            .ok_or_else(|| thrown("The export has no selected path"))?
            .to_owned();
        let (request, admission) = app.owned_export(cx, id, kind, &path)?;
        Ok::<_, IpcError>((id, path, request, admission))
    })();
    let (id, path, request, admission) = match prepared {
        Ok(value) => value,
        Err(error) => return responder.reply(Err(error)),
    };
    let worker_request = request.clone();
    app.offload(
        move || {
            let bytes = produce(&worker_request).map_err(thrown)?;
            main_core::packages::electron_utils::atomic_write::owned::write_file(
                &path, &bytes, admission,
            )
            .map_err(|error| thrown(error.message))?;
            Ok::<_, IpcError>(path)
        },
        move |app, answer| {
            let result = answer.and_then(|path| request.effect(|| Ok(export_written(path))));
            if let Err(error) = &result {
                app.finish_owned_save(id, Err(error.clone()));
            }
            responder.reply(result);
        },
    );
}
