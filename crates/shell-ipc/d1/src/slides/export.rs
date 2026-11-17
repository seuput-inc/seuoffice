//! Export and picker channels, headless export, local fonts, template bytes
//! and cover, microphone, hand-off and generated pages.

use std::time::{Duration, Instant};

use jscompat::{JsonValue, buffer_from_base64};
use main_core::slides::export_images::build_export_image_paths;
use shell_contract::hosts::MessageBox;
use shell_contract::ipc::Target;
use shell_contract::ipc::{CallContext, Responder};
use shell_contract::{IpcError, IpcValue, ViewId};

use super::dialog::{filter, jstr, jstrs, options};
use super::lifecycle::read_file;
use super::session::{Obj, null, prop};
use super::state::{App, NarrationMicrophone};
use crate::value::{Args, as_str, buffer, field, obj, string, thrown};

type Reply = Result<IpcValue, IpcError>;

/// `NARRATION_MICROPHONE_MAX_MS`
const NARRATION_MICROPHONE_MAX: Duration = Duration::from_secs(4 * 60 * 60);

#[path = "export_owned.rs"]
pub(super) mod owned;
pub(super) use owned::export_pdf;

fn node_write(path: &str, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| {
        format!(
            "Error: {}",
            main_core::node_fs::node_error(&e, "open", path).message
        )
    })
}

pub fn pick_export_dir(app: &App, _cx: &CallContext, _args: Args) -> Reply {
    let picked = app.pick_open_first(&options([
        ("title", jstr(app.tm("dlgPickExportDir", &[]))),
        ("buttonLabel", jstr(app.tm("btnExport", &[]))),
        ("properties", jstrs(&["openDirectory", "createDirectory"])),
    ]));
    Ok(picked.map_or(IpcValue::Null, string))
}

/// `slides:ask-withheld-export`: a PDF or image copy records no mark, so the
/// person chooses before anything is written; the renderer acts on the answer
pub fn ask_withheld_export(app: &App, _cx: &CallContext, args: Args) -> Reply {
    let format = match args.str(0) {
        Some(format @ ("PDF" | "PNG" | "JPG")) => format,
        _ => return Err(thrown("Invalid export format.")),
    };
    let with_format = [("format", JsonValue::String(format.to_owned()))];
    let options = MessageBox {
        kind: Some("warning".into()),
        title: None,
        message: app.tm("withheldExportMsg", &with_format),
        detail: Some(app.tm("withheldExportDetail", &with_format)),
        buttons: vec![
            app.tm("withheldExportKeepPptxBtn", &[]),
            app.tm("withheldExportWithoutMarksBtn", &with_format),
            app.tm("btnCancel", &[]),
        ],
        default_id: Some(0),
        cancel_id: Some(2),
        checkbox_label: None,
    };
    Ok(string(
        match app.message_box(app.shell.dialog_parent(), &options) {
            0 => "pptx",
            1 => "export",
            _ => "cancel",
        },
    ))
}

pub fn pick_export_pdf_path(app: &App, _cx: &CallContext, args: Args) -> Reply {
    let mut entries = vec![("title", jstr(app.tm("dlgExportPdf", &[])))];
    if let Some(name) = args.str(0) {
        entries.push(("defaultPath", jstr(name)));
    }
    let mut opts = jscompat::JsObject::new();
    for (key, value) in entries {
        opts.set(key, value);
    }
    opts.set("filters", JsonValue::Array(vec![filter("PDF", &["pdf"])]));
    Ok(app.pick_save(&opts)?.map_or(IpcValue::Null, string))
}

pub fn save_picture(app: &App, _cx: &CallContext, args: Args) -> Reply {
    let op = args.get(0);
    let default_name = match prop(op, "defaultName")? {
        IpcValue::String(name) => name.clone(),
        IpcValue::Undefined => "undefined".to_owned(),
        other => super::session::js_string(other),
    };
    let picked = app.pick_save(&options([
        ("title", jstr(app.tm("dlgSavePicture", &[]))),
        ("defaultPath", jstr(format!("{default_name}.png"))),
        ("filters", JsonValue::Array(vec![filter("PNG", &["png"])])),
    ]))?;
    let Some(path) = picked else {
        return Ok(obj([("ok", IpcValue::Bool(false))]));
    };
    let bytes = buffer_from_base64(as_str(field(op, "pngBase64")).unwrap_or(""));
    Ok(match node_write(&path, &bytes) {
        Ok(()) => obj([("ok", IpcValue::Bool(true)), ("path", string(path))]),
        Err(error) => obj([("ok", IpcValue::Bool(false)), ("error", string(error))]),
    })
}

pub fn export_images(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    let op = args.get(0);
    let failed = |error: String| obj([("ok", IpcValue::Bool(false)), ("error", string(error))]);
    let kind = if as_str(field(op, "format")) == Some("jpg") {
        "jpg"
    } else {
        "png"
    };
    let scope = match app.export_scope(
        cx,
        as_str(field(op, "saveRequestId")),
        kind,
        as_str(field(op, "saveAsTarget")).unwrap_or(""),
    ) {
        Ok(scope) => scope,
        Err(error) => return responder.reply(Ok(failed(error.message))),
    };
    let revision = match scope.revision(app) {
        Ok(revision) => revision,
        Err(error) => return responder.reply(Err(error)),
    };
    let outcome: Result<(Vec<String>, Vec<Vec<u8>>), String> = (|| {
        let format = prop(op, "format").map_err(|e| e.to_string())?;
        let ext = if as_str(format) == Some("jpg") {
            "jpg"
        } else {
            "png"
        };
        let target = field(op, "saveAsTarget");
        let has_target = !matches!(target, IpcValue::Undefined);
        if has_target && ext != "png" {
            return Err("Save As images are PNG".into());
        }
        let pngs: Vec<Vec<u8>> = match field(op, "pngsBase64") {
            IpcValue::Array(items) => items
                .iter()
                .map(|item| buffer_from_base64(as_str(item).unwrap_or("")))
                .collect(),
            _ => {
                return Err(
                    "TypeError: Cannot read properties of undefined (reading 'length')".into(),
                );
            }
        };
        let count = pngs.len() as f64;
        let paths = if has_target {
            let wanted = as_str(target).unwrap_or("").to_owned();
            if as_str(field(op, "saveRequestId")).is_none() {
                return Err("The Save As copy request identifier is required".into());
            }
            Some(
                main_core::packages::electron_utils::numbered_export_paths::numbered_export_paths(
                    &wanted, count,
                )
                .map_err(|error| format!("RangeError: {}", error.0))?,
            )
        } else {
            Some(build_export_image_paths(
                as_str(field(op, "dir")).unwrap_or(""),
                as_str(field(op, "baseName")).unwrap_or(""),
                count,
                ext,
            ))
        };
        let Some(paths) = paths else {
            return Err("export target was not authorized".into());
        };
        Ok((paths, pngs))
    })();
    let (paths, pngs) = match outcome {
        Ok(ok) => ok,
        Err(error) => return responder.reply(Ok(failed(error))),
    };
    let admissions = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            scope.revision_numbered_admission(
                app,
                revision.clone(),
                path.clone(),
                pngs.len(),
                index,
            )
        })
        .collect::<Vec<_>>();
    let services = app.services.clone();
    let index = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let outcome =
            paths
                .iter()
                .zip(&pngs)
                .zip(admissions)
                .try_for_each(|((path, bytes), admission)| {
                    main_core::packages::electron_utils::atomic_write::owned::write_file(
                        path, bytes, admission,
                    )
                });
        super::post_to(&services, index, move |app| {
            let result = scope.effect_at_revision(app, &revision, || match outcome {
                Ok(()) => obj([
                    ("ok", IpcValue::Bool(true)),
                    (
                        "paths",
                        IpcValue::Array(paths.into_iter().map(string).collect()),
                    ),
                ]),
                // Electron writes each file in place, so its `String(err)` names the
                // target; the staged write names its temporary file beside it
                Err(error) => obj([
                    ("ok", IpcValue::Bool(false)),
                    (
                        "error",
                        string(format!(
                            "Error: {}",
                            without_stage_name(&error.message, &paths)
                        )),
                    ),
                ]),
            });
            responder.reply(result);
        });
    }));
}

/// `message` with the staging file's name (`<target>.<12 hex>.tmp`) replaced by the target's.
fn without_stage_name(message: &str, paths: &[String]) -> String {
    let mut text = message.to_owned();
    for path in paths {
        let mut from = 0;
        while let Some(found) = text[from..].find(path.as_str()) {
            let start = from + found + path.len();
            let stage = text[start..].strip_prefix('.').filter(|rest| {
                rest.get(..12)
                    .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                    && rest[12..].starts_with(".tmp")
            });
            if stage.is_some() {
                text.replace_range(start..start + 17, "");
            }
            from = start;
        }
    }
    text
}

pub fn consume_headless_export(app: &App, cx: &CallContext, _args: Args) -> Reply {
    let owner = super::session::render_owner(cx)?;
    let Some(target) = app
        .shell
        .take_headless_target_owned(&owner, &cx.cancel)
        .map_err(thrown)?
    else {
        return Ok(IpcValue::Null);
    };
    let effects = app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .ok_or_else(|| thrown("The native export effect host is unavailable"))?;
    effects
        .with_native_invocation_effect(
            &target.invocation,
            &owner,
            std::slice::from_ref(&owner),
            &cx.cancel,
            &cx.cancel,
            &mut || {
                app.state
                    .borrow_mut()
                    .headless_targets
                    .insert(owner.view, target.invocation.clone());
                Ok(())
            },
        )
        .map_err(thrown)?;
    Ok(string(target.path))
}

pub fn headless_export_done(app: &App, cx: &CallContext, args: Args) -> Result<(), IpcError> {
    let result = args.get(0);
    let ok = matches!(field(result, "ok"), IpcValue::Bool(true));
    let error = as_str(field(result, "error")).map(str::to_owned);
    let owner = super::session::render_owner(cx)?;
    app.shell
        .headless_export_done_owned(&owner, &cx.cancel, ok, error)
        .map_err(thrown)?;
    let mut state = app.state.borrow_mut();
    if state
        .headless_targets
        .get(&owner.view)
        .is_some_and(|document| {
            owner
                .context
                .renderer()
                .is_some_and(|renderer| document.created().same_generation(renderer))
        })
    {
        state.headless_targets.remove(&owner.view);
    }
    Ok(())
}

pub fn font_install_local(app: &App, _cx: &CallContext, _args: Args) -> Reply {
    let paths = app.pick_open(&options([
        ("properties", jstrs(&["openFile", "multiSelections"])),
        (
            "filters",
            JsonValue::Array(vec![filter("Fonts", &["ttf", "otf", "ttc", "otc"])]),
        ),
    ]));
    if paths.is_empty() {
        return Ok(obj([("families", IpcValue::Array(Vec::new()))]));
    }
    let user_data = crate::host::path_string(
        app.services
            .paths
            .path(shell_contract::runtime::PathName::UserData),
    );
    let engine = app.engine.clone();
    let families = main_core::slides::font_store::install_local_font_files(
        &mut || user_data.clone(),
        &|path| {
            engine
                .font_file_families(path)
                .map_err(|error| main_core::node_fs::NodeError {
                    code: None,
                    message: error.0,
                })
        },
        &paths,
    )
    .map_err(|e| thrown(e.message))?;
    if !families.is_empty() {
        app.after_fonts_changed()?;
    }
    Ok(obj([(
        "families",
        IpcValue::Array(families.into_iter().map(string).collect()),
    )]))
}

impl App {
    /// `afterFontsChanged()`: rebuild every open session with a fresh
    /// registry and push the relaid-out slides to all attached windows
    fn after_fonts_changed(&self) -> Result<(), IpcError> {
        self.engine
            .reset_font_metrics()
            .map_err(|error| thrown(error.0))?;
        let sends: Vec<(super::engine::RenderOwner, IpcValue)> = {
            let state = self.state.borrow();
            let mut sends = Vec::new();
            for slot in state.live_slots() {
                let Some(session) = state.slot(slot) else {
                    continue;
                };
                for owner in state.attached_owners(slot) {
                    let payload = obj([
                        ("slides", self.render_all(session, &owner)?),
                        (
                            "size",
                            self.size_value(
                                self.engine.size(session.deck()).map_err(|e| thrown(e.0))?,
                            ),
                        ),
                    ]);
                    sends.push((owner, payload));
                }
            }
            sends
        };
        for (owner, payload) in sends {
            self.services
                .events
                .emit_to_owner_checked(&owner, "slides:deck-changed", vec![payload])
                .map_err(thrown)?;
        }
        self.services
            .events
            .emit(Target::All, "slides:fonts-changed", Vec::new());
        Ok(())
    }
}

pub fn private_font_faces(app: &App, cx: &CallContext, _args: Args) -> Reply {
    let owner = super::session::render_owner(cx)?;
    app.engine
        .private_font_faces(&owner)
        .map_err(|error| thrown(error.0))
}

pub fn private_font_data(app: &App, cx: &CallContext, args: Args) -> Reply {
    let owner = super::session::render_owner(cx)?;
    app.engine
        .private_font_data(&owner, args.get(0))
        .map_err(|error| thrown(error.0))
}

// ---- template bytes and cover ----

pub fn template_bytes(app: &App, cx: &CallContext, _args: Args) -> Reply {
    let state = app.state.borrow();
    let Some(s) = state.session(cx.sender.view) else {
        // `track` runs the write: no deck, no bytes
        return null();
    };
    // refused while an AI run's unaccepted edits are in the deck: null
    if s.core.ai_run {
        return null();
    }
    Ok(
        match app.engine.save_bytes(s.deck()).and_then(|bytes| {
            app.engine
                .has_redaction(s.deck())
                .map(|redacted| (bytes, redacted))
        }) {
            Ok((bytes, redacted)) => obj([
                ("bytes", buffer(bytes)),
                ("redacted", IpcValue::Bool(redacted)),
            ]),
            Err(_) => IpcValue::Null,
        },
    )
}

pub fn template_cover_render(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    let owner = match super::session::render_owner(cx) {
        Ok(owner) => owner,
        Err(error) => return responder.reply(Err(error)),
    };
    let (Some(id), Some(lang)) = (args.str(0), args.str(1)) else {
        return responder.reply(null());
    };
    if lang != "ko" && lang != "en" {
        return responder.reply(null());
    }
    let (id, lang) = (id.to_owned(), lang.to_owned());
    let engine = app.engine.clone();
    app.services.workers.spawn(Box::new(move || {
        let result = engine
            .template_cover(&owner, &id, &lang, 1280.0)
            .map(|cover| cover.unwrap_or(IpcValue::Null))
            .map_err(|error| thrown(error.0));
        if !owner
            .context
            .renderer()
            .is_some_and(|generation| generation.is_active())
        {
            return responder.reply(Err(thrown("renderer capability is missing or revoked")));
        }
        responder.reply(result);
    }));
}

// ---- narration microphone ----

impl App {
    /// `closeNarrationMicrophone(wcId)`
    pub fn close_narration_microphone(&self, view: ViewId) {
        self.state.borrow_mut().narration.remove(&view);
    }

    /// `slidesMicrophoneOpen(wcId)`: true from the moment this editor's
    /// narration recording was let start until it stops
    pub fn microphone_open(&self, view: ViewId) -> bool {
        self.state
            .borrow()
            .narration
            .get(&view)
            .and_then(|m| m.deadline)
            .is_some_and(|deadline| Instant::now() < deadline)
    }
}

pub fn microphone_access(app: &App, cx: &CallContext, _args: Args, responder: Responder) {
    let view = cx.sender.view;
    let ticket = {
        let mut state = app.state.borrow_mut();
        // only an editor with a deck open records
        if !state.has_session(view) {
            return responder.reply(Ok(IpcValue::Bool(false)));
        }
        state.next_ticket += 1;
        let ticket = state.next_ticket;
        state.narration.insert(
            view,
            NarrationMicrophone {
                deadline: None,
                ticket,
            },
        );
        ticket
    };
    let index = app.registry_index;
    let services = app.services.clone();
    app.shell.ask_microphone(Box::new(move |granted| {
        super::post_to(&services, index, move |app| {
            let granted = match granted {
                Ok(granted) => granted,
                Err(error) => {
                    let mut state = app.state.borrow_mut();
                    if state
                        .narration
                        .get(&view)
                        .is_some_and(|recording| recording.ticket == ticket)
                    {
                        state.narration.remove(&view);
                    }
                    return responder.reply(Err(thrown(error)));
                }
            };
            let reply = {
                let mut state = app.state.borrow_mut();
                // Stop, navigation or a newer recording invalidates this OS question
                if state.narration.get(&view).map(|m| m.ticket) != Some(ticket) {
                    false
                } else if !granted || !state.has_session(view) {
                    state.narration.remove(&view);
                    false
                } else {
                    if let Some(recording) = state.narration.get_mut(&view) {
                        recording.deadline = Some(Instant::now() + NARRATION_MICROPHONE_MAX);
                    }
                    true
                }
            };
            responder.reply(Ok(IpcValue::Bool(reply)));
        })
    }));
}

pub fn microphone_release(app: &App, cx: &CallContext, _args: Args) -> Result<(), IpcError> {
    app.close_narration_microphone(cx.sender.view);
    Ok(())
}

// ---- generate hand-off ----

pub fn handoff_take(app: &App, cx: &CallContext, _args: Args) -> Reply {
    let owner = super::session::render_owner(cx)?;
    Ok(
        match app
            .shell
            .take_generate_handoff_owned(&owner, &cx.cancel)
            .map_err(thrown)?
        {
            None => IpcValue::Null,
            Some(handoff) => obj([
                ("id", string(handoff.id)),
                ("text", string(handoff.text)),
                (
                    "materials",
                    IpcValue::Array(
                        handoff
                            .materials
                            .into_iter()
                            .map(|(path, name)| {
                                obj([("path", string(path)), ("name", string(name))])
                            })
                            .collect(),
                    ),
                ),
            ]),
        },
    )
}

/// a run carries at most this many attachment paths in one report
const MAX_REPORTED_PATHS: usize = 200;

pub fn handoff_run_started(app: &App, cx: &CallContext, args: Args) -> Reply {
    let IpcValue::Array(items) = args.get(0) else {
        return Ok(IpcValue::Bool(false));
    };
    let paths: Vec<String> = items
        .iter()
        .take(MAX_REPORTED_PATHS)
        .filter_map(|p| as_str(p).filter(|s| !s.is_empty()).map(str::to_owned))
        .collect();
    if paths.is_empty() {
        return Ok(IpcValue::Bool(false));
    }
    Ok(IpcValue::Bool(
        app.shell
            .report_generate_run_started_owned(
                &super::session::render_owner(cx)?,
                &cx.cancel,
                &paths,
            )
            .map_err(thrown)?,
    ))
}

// ---- generated pages ----

const CLOUD_PAGE_PREFIX: &str = "cloudpptx:";

pub fn local_page_generate(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    let owner = match super::session::render_owner(cx) {
        Ok(owner) => owner,
        Err(error) => return responder.reply(Err(error)),
    };
    let op = args.get(0);
    let spec_json = match field(op, "specJson") {
        IpcValue::String(text) => text.clone(),
        IpcValue::Undefined | IpcValue::Null => String::new(),
        other => super::session::js_string(other),
    };
    let default_fonts = field(op, "defaultFonts").clone();
    let engine = app.engine.clone();
    let shell = app.shell.clone();
    let temp = app
        .services
        .paths
        .path(shell_contract::runtime::PathName::Temp);
    let index = app.registry_index;
    let services = app.services.clone();
    app.services.workers.spawn(Box::new(move || {
        let failed = |error: String| obj([("ok", IpcValue::Bool(false)), ("error", string(error))]);
        let page = match engine.generate_page(&owner, &spec_json, &default_fonts) {
            Ok(page) => page,
            Err(error) => return responder.reply(Ok(failed(error))),
        };
        let dir = temp.join("seuoffice-local-pages");
        let path = dir.join(format!("{}.pptx", shell.random_uuid()));
        let path_text = path.to_string_lossy().into_owned();
        if let Err(error) =
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &page.bytes))
        {
            return responder.reply(Ok(failed(error.to_string())));
        }
        let reply = Obj::new()
            .set("ok", IpcValue::Bool(true))
            .set("marker", string(format!("{CLOUD_PAGE_PREFIX}{path_text}")))
            .set_if(!page.image_failures.is_empty(), "imageFailures", || {
                IpcValue::Array(
                    page.image_failures
                        .iter()
                        .map(|f| string(f.as_str()))
                        .collect(),
                )
            })
            .build();
        super::post_to(&services, index, move |app| {
            app.state.borrow_mut().issued_cloud_pages.insert(path_text);
            responder.reply(Ok(reply));
        });
    }));
}

impl App {
    /// `readCloudPage(marker)`: the bytes of a page this process issued
    fn read_cloud_page(&self, marker: &IpcValue) -> Result<Vec<u8>, IpcError> {
        let Some(rest) = as_str(marker).and_then(|m| m.strip_prefix(CLOUD_PAGE_PREFIX)) else {
            return Err(thrown("expected a generated page marker"));
        };
        if !self.state.borrow().issued_cloud_pages.contains(rest) {
            return Err(thrown("unknown generated page marker"));
        }
        read_file(rest)
    }

    pub fn page_marker_list<'a>(&self, markers: &'a IpcValue) -> &'a [IpcValue] {
        match markers {
            IpcValue::Array(items) => items,
            _ => &[],
        }
    }

    pub fn read_cloud_pages(&self, markers: &IpcValue) -> Result<Vec<Vec<u8>>, IpcError> {
        self.page_marker_list(markers)
            .iter()
            .map(|m| self.read_cloud_page(m))
            .collect()
    }

    pub fn read_one_cloud_page(&self, marker: &IpcValue) -> Result<Vec<u8>, IpcError> {
        self.read_cloud_page(marker)
    }
}

#[cfg(test)]
mod stage_name_tests {
    use super::without_stage_name;

    #[test]
    fn the_staging_name_gives_way_to_the_target() {
        let paths = vec!["/a/missing/slide-01.png".to_owned()];
        assert_eq!(
            without_stage_name(
                "ENOENT: no such file or directory, open '/a/missing/slide-01.png.a6e3b99b476b.tmp'",
                &paths,
            ),
            "ENOENT: no such file or directory, open '/a/missing/slide-01.png'"
        );
        assert_eq!(
            without_stage_name("EACCES: permission denied", &paths),
            "EACCES: permission denied"
        );
        assert_eq!(
            without_stage_name("open '/a/missing/slide-01.png.keep'", &paths),
            "open '/a/missing/slide-01.png.keep'"
        );
    }
}
