//! The existing export algorithms with the retained close-save authority.
use super::*;
use crate::docs::owned_save::{self, Scope};
use crate::staged::{Published, StagedOutput};
use main_core::packages::electron_utils::atomic_write::atomic_write_file_authorized;

type Pick = Box<dyn FnOnce(Result<Option<String>, Reply>)>;

#[allow(clippy::too_many_arguments)]
fn target(
    d: &Rc<Docs>,
    scope: Scope,
    out: &IpcValue,
    name: &IpcValue,
    title: &str,
    extension: &str,
    label: &str,
    markdown: bool,
    then: Pick,
) {
    if let Err(error) = scope.check() {
        return then(Err(Err(kit::error(error))));
    }
    match named_target(d, scope.destination().view, out) {
        Err(reply) => return then(Err(Ok(reply))),
        Ok(Some(path)) => return then(Ok(Some(path))),
        Ok(None) => {}
    }
    let stem = match default_stem(
        name,
        if markdown {
            js_regex!(r"\.docx$", "i")
        } else {
            js_regex!(r"\.(?:docx|odt)$", "i")
        },
    ) {
        Ok(stem) => stem,
        Err(reply) => return then(Err(reply)),
    };
    let options = js_obj(vec![
        ("title", js_str(d.tm(title))),
        ("defaultPath", js_str(format!("{stem}.{extension}"))),
        (
            "filters",
            js_filters(&[(label.to_owned(), vec![extension])]),
        ),
    ]);
    let completed = d.clone();
    let checked = scope.clone();
    owned_save::save_dialog(
        d,
        scope,
        options,
        None,
        Box::new(move |result| {
            let result = match result {
                Ok(result) => result,
                Err(error) => return then(Err(Err(error))),
            };
            let Some(path) = picked_path(&result, "filePath") else {
                return then(Ok(None));
            };
            match checked.effect(|| completed.allow_pdf_write(checked.destination().view, &path)) {
                Ok(()) => then(Ok(Some(path))),
                Err(error) => then(Err(Err(kit::error(error)))),
            }
        }),
    );
}
fn write(scope: &Scope, path: &str, bytes: &[u8]) -> Result<(), String> {
    atomic_write_file_authorized(path, bytes, &mut |effect| {
        scope
            .effect_path(path, effect)
            .map_err(|message| main_core::node_fs::NodeError {
                code: None,
                message,
            })
    })
    .map_err(|error| node_text(&error))
}
fn stage(path: &str, bytes: Vec<u8>) -> Result<StagedOutput, String> {
    StagedOutput::prepare(
        std::path::Path::new(path),
        bytes,
        file_commit::PublicationMode::Replace,
    )
}
fn finish(
    d: &Rc<Docs>,
    scope: Scope,
    path: String,
    dropped: Option<usize>,
    reveal: bool,
    published: Published,
    responder: Responder,
) {
    let view = scope.destination().view;
    let temporary = d.is_image_temp(view, &path);
    if let Err(error) = scope.check() {
        return responder.reply(Err(kit::error(error)));
    }
    if !temporary
        && let Err(error) = scope.note_landed(&path)
    {
        return responder.reply(Err(kit::error(error)));
    }
    let mut answer = Obj::new().set("ok", true).set("path", path.clone());
    if let Some(dropped) = dropped {
        answer = answer.set("imagesDropped", dropped as f64);
    }
    let answer = answer.done();
    if !reveal || temporary || d.shell.is_headless() {
        return responder.reply(Ok(answer));
    }
    let os = d.os.clone();
    let completed = scope.clone();
    let mailbox_docs = d.clone();
    let Some(ticket) = crate::docs::cleanup_mailbox::begin(
        d,
        Box::new(move |opened, settled| match opened {
            Ok(shell_contract::hosts::GeneratedPathOutcome::Opened(receipt)) => {
                finish_generated(completed, receipt, answer, responder);
                settled.complete();
            }
            Err(error) => {
                responder.reply(Err(kit::error(error)));
                settled.complete();
            }
            Ok(shell_contract::hosts::GeneratedPathOutcome::CleanupPending(_)) => {
                responder.reply(Err(kit::error(
                    "The generated export has an invalid nested cleanup",
                )));
                settled.complete();
            }
            Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened) => {
                let after_reveal = completed.clone();
                let reveal_path = path.clone();
                let reveal_done = crate::docs::cleanup_mailbox::callback(
                    &mailbox_docs,
                    settled.id(),
                    move |result: Result<(), String>| {
                        if let Err(error) = result {
                            responder.reply(Err(kit::error(error)));
                            settled.complete();
                            return;
                        }
                        let receipt = match after_reveal.origin() {
                        Some(origin) => shell_contract::hosts::CommittedGeneratedPath::from_loaded_target(
                            origin.clone(), after_reveal.destination().clone(), after_reveal.destination().clone(),
                            after_reveal.cancel().clone(), after_reveal.pending().clone(), after_reveal.effects().clone()),
                        None => after_reveal.selected_destination(&reveal_path).and_then(|selection|
                            shell_contract::hosts::CommittedGeneratedPath::from_existing_document_save(
                                selection, after_reveal.destination().clone(), after_reveal.all_participants(),
                                after_reveal.cancel().clone(), after_reveal.pending().clone(),
                                after_reveal.additional_cancellation().to_vec(), after_reveal.effects().clone())),
                    };
                        match receipt {
                            Ok(receipt) => finish_generated(
                                after_reveal,
                                std::sync::Arc::new(receipt),
                                answer,
                                responder,
                            ),
                            Err(error) => responder.reply(Err(kit::error(error))),
                        }
                        settled.complete();
                    },
                );
                let action = Box::new(move || {
                    os.show_item_in_folder(std::path::Path::new(&path));
                    Ok(())
                });
                match (completed.origin(), completed.native_binding()) {
                    (Some(origin), _) => completed.effects().post_ui_owned_many_extra(
                        origin.clone(),
                        completed.all_participants(),
                        completed.cancel().clone(),
                        completed.pending().clone(),
                        completed.additional_cancellation().to_vec(),
                        action,
                        reveal_done,
                    ),
                    (_, Some(binding)) => completed.effects().post_ui_existing_document_save_extra(
                        binding.clone(),
                        completed.all_participants(),
                        completed.cancel().clone(),
                        completed.pending().clone(),
                        completed.additional_cancellation().to_vec(),
                        action,
                        reveal_done,
                    ),
                    _ => reveal_done(Err("the export authority is unavailable".into())),
                }
            }
        }),
    ) else {
        return;
    };
    let (output, input) = published;
    match d.shell.prepare_generated_output(
        output,
        Some(input),
        scope.publication_participants(),
        scope.cancel().clone(),
        scope.pending().clone(),
        scope.additional_cancellation().to_vec(),
        scope.effects().clone(),
        scope.reload_accepted(),
        ticket.wake(),
    ) {
        Ok(prepared) => ticket.attach(d, prepared),
        Err(error) => ticket.fail(d, error),
    }
}

fn finish_generated(
    scope: Scope,
    receipt: std::sync::Arc<shell_contract::hosts::CommittedGeneratedPath>,
    answer: IpcValue,
    responder: Responder,
) {
    if let Err(error) = scope.retain_generated_receipt(receipt) {
        return responder.reply(Err(kit::error(error)));
    }
    let mut accepted = false;
    let admitted = scope.with_generated_terminal_commit(&mut || {
        accepted = true;
        Ok(())
    });
    match admitted {
        Ok(()) if accepted => responder.reply(Ok(answer)),
        Ok(()) => responder.reply(Err(kit::error("The generated export was not admitted"))),
        Err(error) => responder.reply(Err(kit::error(error))),
    }
}
fn geometry(args: &kit::Args, w: usize, h: usize, scale: usize) -> Result<IpcValue, IpcValue> {
    let (width, height, scale_value) = (
        kit::to_js_json(args.get(w)),
        kit::to_js_json(args.get(h)),
        kit::to_js_json(args.get(scale)),
    );
    if !valid_print_geometry(width.as_ref(), height.as_ref(), scale_value.as_ref()) {
        return Err(failed("invalid page size or scale"));
    }
    let mut options = vec![
        ("printBackground".into(), true.into_ipc()),
        (
            "pageSize".into(),
            Obj::new()
                .set("width", args.num(w).unwrap_or(0.0) / TWIPS_PER_INCH)
                .set("height", args.num(h).unwrap_or(0.0) / TWIPS_PER_INCH)
                .done(),
        ),
        (
            "margins".into(),
            Obj::new()
                .set("top", 0.0)
                .set("bottom", 0.0)
                .set("left", 0.0)
                .set("right", 0.0)
                .done(),
        ),
    ];
    if let Some(scale) = print_scale_option(scale_value.as_ref()) {
        options.push(("scale".into(), scale.into_ipc()));
    }
    Ok(IpcValue::Object(options))
}
pub(super) fn print_buffer(d: &Rc<Docs>, scope: Scope, args: kit::Args, responder: Responder) {
    let options = match geometry(&args, 0, 1, 2) {
        Ok(options) => options,
        Err(reply) => return responder.reply(Ok(reply)),
    };
    let contents = d.contents.clone();
    let completed = scope.clone();
    kit::on_worker(
        &d.services,
        move || scope.print_to_pdf(contents.as_ref(), options),
        move |result| {
            if let Err(error) = completed.check() {
                return responder.reply(Err(kit::error(error)));
            }
            responder.reply(Ok(match result {
                Ok(bytes) => Obj::new()
                    .set("ok", true)
                    .set("base64", base64(&bytes))
                    .done(),
                Err(error) => failed(error),
            }));
        },
    );
}
pub(super) fn pdf(d: &Rc<Docs>, scope: Scope, args: kit::Args, responder: Responder) {
    let options = match geometry(&args, 1, 2, 4) {
        Ok(options) => options,
        Err(reply) => return responder.reply(Ok(reply)),
    };
    let links = match args.get(5) {
        IpcValue::Array(items) if !items.is_empty() => kit::to_json(args.get(5)),
        _ => None,
    };
    let docs = d.clone();
    let picked_scope = scope.clone();
    target(
        d,
        scope,
        args.get(3),
        args.get(0),
        "dlgExportPdf",
        "pdf",
        "PDF",
        false,
        Box::new(move |picked| {
            let path = match picked {
                Ok(Some(path)) => path,
                Ok(None) => return responder.reply(Ok(not_ok())),
                Err(reply) => return responder.reply(reply),
            };
            let target = path.clone();
            let worker = picked_scope.clone();
            let completed = docs.clone();
            let contents = docs.contents.clone();
            let engine = docs.engine.clone();
            kit::on_worker(
                &docs.services,
                move || {
                    let printed = worker.print_to_pdf(contents.as_ref(), options)?;
                    let bytes = match links {
                        Some(links) => engine
                            .add_pdf_internal_links(printed, links)
                            .map_err(|error| error_text(&error))?,
                        None => printed,
                    };
                    stage(&target, bytes)
                },
                move |result| {
                    let published =
                        result.and_then(|staged| picked_scope.publish_staged(&path, staged));
                    match published {
                        Ok(published) => finish(
                            &completed,
                            picked_scope,
                            path,
                            None,
                            true,
                            published,
                            responder,
                        ),
                        Err(error) => responder.reply(Ok(failed_at(error, &path))),
                    }
                },
            );
        }),
    );
}
pub(super) fn merged(d: &Rc<Docs>, scope: Scope, args: kit::Args, responder: Responder) {
    let parts = match args.get(1) {
        IpcValue::Array(parts) => Some(parts.iter().map(js_string).collect::<Vec<_>>()),
        _ => None,
    };
    let links = match args.get(3) {
        IpcValue::Array(items) if !items.is_empty() => kit::to_json(args.get(3)),
        _ => None,
    };
    let docs = d.clone();
    let picked_scope = scope.clone();
    target(
        d,
        scope,
        args.get(2),
        args.get(0),
        "dlgExportPdf",
        "pdf",
        "PDF",
        false,
        Box::new(move |picked| {
            let path = match picked {
                Ok(Some(path)) => path,
                Ok(None) => return responder.reply(Ok(not_ok())),
                Err(reply) => return responder.reply(reply),
            };
            let target = path.clone();
            let worker = picked_scope.clone();
            let completed = docs.clone();
            let engine = docs.engine.clone();
            kit::on_worker(
                &docs.services,
                move || {
                    worker.check()?;
                    let parts = parts
                        .ok_or_else(|| "TypeError: base64Parts is not iterable".to_owned())?
                        .iter()
                        .map(|part| buffer_from_base64(part))
                        .collect();
                    let bytes = engine
                        .merge_pdfs(parts, links)
                        .map_err(|error| error_text(&error))?;
                    stage(&target, bytes)
                },
                move |result| {
                    let published =
                        result.and_then(|staged| picked_scope.publish_staged(&path, staged));
                    match published {
                        Ok(published) => finish(
                            &completed,
                            picked_scope,
                            path,
                            None,
                            true,
                            published,
                            responder,
                        ),
                        Err(error) => responder.reply(Ok(failed(error))),
                    }
                },
            );
        }),
    );
}
pub(super) fn html(d: &Rc<Docs>, scope: Scope, args: kit::Args, responder: Responder) {
    let Some(html) = args
        .str(1)
        .filter(|html| !html.is_empty())
        .map(str::to_owned)
    else {
        return responder.reply(Ok(failed("empty document")));
    };
    let Some(permit) = d.media_permit(scope.destination().view) else {
        return responder.reply(Err(kit::error(
            "The document media session is unavailable.",
        )));
    };
    let docs = d.clone();
    let picked_scope = scope.clone();
    target(
        d,
        scope,
        args.get(2),
        args.get(0),
        "dlgExportHtml",
        "html",
        "HTML",
        false,
        Box::new(move |picked| {
            let path = match picked {
                Ok(Some(path)) => path,
                Ok(None) => return responder.reply(Ok(not_ok())),
                Err(reply) => return responder.reply(reply),
            };
            let target = path.clone();
            let worker = picked_scope.clone();
            let completed = docs.clone();
            let engine = docs.engine.clone();
            kit::on_worker(
                &docs.services,
                move || {
                    worker.check()?;
                    let inlined = inline_lazy_media_in_html(
                        &html,
                        &mut lazy_reader(&engine, &permit, worker.cancel()),
                    )
                    .map_err(|error| error_text(&error))?;
                    stage(&target, inlined.into_bytes())
                },
                move |result| {
                    let published =
                        result.and_then(|staged| picked_scope.publish_staged(&path, staged));
                    match published {
                        Ok(published) => finish(
                            &completed,
                            picked_scope,
                            path,
                            None,
                            true,
                            published,
                            responder,
                        ),
                        Err(error) => responder.reply(Ok(failed_at(error, &path))),
                    }
                },
            );
        }),
    );
}
pub(super) fn markdown(d: &Rc<Docs>, scope: Scope, args: kit::Args, responder: Responder) {
    let Some(markdown) = args
        .str(1)
        .filter(|markdown| !markdown.is_empty())
        .map(str::to_owned)
    else {
        return responder.reply(Ok(failed("empty document")));
    };
    let IpcValue::Array(images) = args.get(2) else {
        return responder.reply(Ok(failed("bad image list")));
    };
    let images = images
        .iter()
        .map(|image| {
            (
                kit::as_str(kit::field(image, "name")).map(str::to_owned),
                kit::as_str(kit::field(image, "src")).map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    let Some(permit) = d.media_permit(scope.destination().view) else {
        return responder.reply(Err(kit::error(
            "The document media session is unavailable.",
        )));
    };
    let docs = d.clone();
    let picked_scope = scope.clone();
    target(
        d,
        scope,
        args.get(3),
        args.get(0),
        "dlgExportMarkdown",
        "md",
        "Markdown",
        true,
        Box::new(move |picked| {
            let path = match picked {
                Ok(Some(path)) => path,
                Ok(None) => return responder.reply(Ok(not_ok())),
                Err(reply) => return responder.reply(reply),
            };
            let target = path.clone();
            let worker = picked_scope.clone();
            let completed = docs.clone();
            let engine = docs.engine.clone();
            kit::on_worker(
                &docs.services,
                move || {
                    worker.check()?;
                    let stem =
                        js_regex!(r"\.md$", "i").replace_all(&basename(&target), |_| String::new());
                    let directory = join(&[&dirname(&target), &format!("{stem}_files")]);
                    let mut written = 0;
                    for (name, src) in &images {
                        let (Some(name), Some(src)) = (name, src) else {
                            continue;
                        };
                        if !js_regex!(r"^image\d+\.\w+$").test(name) {
                            continue;
                        }
                        let body = match js_regex!(r"^data:image\/[\w.+-]+;base64,(.*)$", "s")
                            .exec(src)
                            .and_then(|found| found.get(1).map(str::to_owned))
                        {
                            Some(base64) => buffer_from_base64(&base64),
                            None => match engine.read_lazy_media(
                                src,
                                permit.clone(),
                                worker.cancel().clone(),
                            ) {
                                Ok(Some(media)) => media.body,
                                Err(error)
                                    if worker.cancel().is_cancelled() || !permit.is_active() =>
                                {
                                    return Err(error);
                                }
                                _ => Vec::new(),
                            },
                        };
                        if body.is_empty() {
                            continue;
                        }
                        if written == 0 {
                            worker.ensure_directory(std::path::Path::new(&directory))?;
                        }
                        write(&worker, &join(&[&directory, name]), &body)?;
                        written += 1;
                    }
                    let media_directory = encode_uri_component(&format!("{stem}_files"))
                        .replace('(', "%28")
                        .replace(')', "%29");
                    let text = markdown
                        .split(&format!("{MARKDOWN_MEDIA_DIR_TOKEN}/"))
                        .collect::<Vec<_>>()
                        .join(&format!("{media_directory}/"));
                    let staged = stage(&target, text.into_bytes())?;
                    Ok((images.len() - written, staged))
                },
                move |result| {
                    let landed = result.and_then(|(dropped, staged)| {
                        picked_scope
                            .publish_staged(&path, staged)
                            .map(|published| (dropped, published))
                    });
                    match landed {
                        Ok((dropped, published)) => finish(
                            &completed,
                            picked_scope,
                            path,
                            Some(dropped),
                            true,
                            published,
                            responder,
                        ),
                        Err(error) => responder.reply(Ok(failed_at(error, &path))),
                    }
                },
            );
        }),
    );
}

pub(super) fn hwp(d: &Rc<Docs>, scope: Scope, args: kit::Args, responder: Responder) {
    let view = scope.destination().view;
    if d.is_torn_down(view) {
        return responder.reply(Ok(not_ok()));
    }
    let Some(permit) = d.media_permit(view) else {
        return responder.reply(Err(kit::error(
            "The document media session is unavailable.",
        )));
    };
    let (Some(name), Some(bytes)) = (
        args.str(0).map(str::to_owned),
        super::super::array_buffer(args.get(1)),
    ) else {
        return responder.reply(Ok(bad_hwp_request()));
    };
    let bytes = bytes.to_vec();
    let password = args.get(3) == &IpcValue::Bool(true);
    match args.get(2) {
        IpcValue::String(path) => {
            if !d.can_pdf_write(view, path) {
                return responder.reply(Ok(Obj::new()
                    .set("ok", false)
                    .set("code", "unauthorized")
                    .set("error", "export target is not an authorized path")
                    .done()));
            }
            let format = if js_regex!(r"\.hwpx$", "i").test(path) {
                HwpFormat::Hwpx
            } else {
                HwpFormat::Hwp
            };
            convert_hwp(
                d,
                scope,
                path.clone(),
                format,
                bytes,
                permit,
                password,
                responder,
            );
        }
        IpcValue::Undefined | IpcValue::Null => {
            let first = match read_hwp_default_format(&Paths(d.services.paths.as_ref())) {
                HwpDefaultFormat::Hwpx => HwpExportFormat::Hwpx,
                HwpDefaultFormat::Hwp => HwpExportFormat::Hwp,
            };
            let filters = hwp_export_filters(first, &mut |key| d.tm(key))
                .into_iter()
                .map(|filter| (filter.name, filter.extensions))
                .collect::<Vec<_>>();
            let stem = js_regex!(r"\.(?:docx|odt)$", "i").replace_all(&name, |_| String::new());
            let options = js_obj(vec![
                ("title", js_str(d.tm("dlgExportHwp"))),
                ("defaultPath", js_str(format!("{stem}.{}", first.as_str()))),
                ("filters", js_filters(&filters)),
            ]);
            let docs = d.clone();
            let checked = scope.clone();
            owned_save::save_dialog(
                d,
                scope,
                options,
                None,
                Box::new(move |picked| {
                    let picked = match picked {
                        Ok(picked) => picked,
                        Err(error) => return responder.reply(Err(error)),
                    };
                    let Some(path) = picked_path(&picked, "filePath") else {
                        return responder.reply(Ok(canceled()));
                    };
                    let (path, format) = hwp_export_target(&path, first);
                    if let Err(error) = checked.effect(|| docs.allow_pdf_write(view, &path)) {
                        return responder.reply(Err(kit::error(error)));
                    }
                    convert_hwp(
                        &docs,
                        checked,
                        path,
                        match format {
                            HwpExportFormat::Hwp => HwpFormat::Hwp,
                            HwpExportFormat::Hwpx => HwpFormat::Hwpx,
                        },
                        bytes,
                        permit,
                        password,
                        responder,
                    );
                }),
            );
        }
        _ => responder.reply(Ok(bad_hwp_request())),
    }
}
#[allow(clippy::too_many_arguments)]
fn convert_hwp(
    d: &Rc<Docs>,
    scope: Scope,
    path: String,
    format: HwpFormat,
    bytes: Vec<u8>,
    permit: MediaPermit,
    password: bool,
    responder: Responder,
) {
    let engine = d.engine.clone();
    let worker = scope.clone();
    let docs = d.clone();
    kit::on_worker(
        &d.services,
        move || {
            worker.check().map_err(HwpError::Error)?;
            let whole = engine
                .materialize_lazy_docx(bytes, permit, worker.cancel().clone())
                .map_err(HwpError::Error)?
                .bytes;
            worker.check().map_err(HwpError::Error)?;
            engine.convert_to_hwp(whole, format)
        },
        move |converted| {
            let converted = match converted {
                Ok(converted) => converted,
                Err(error) => return responder.reply(Ok(hwp_failed(&path, &error))),
            };
            if let Err(error) = scope.check() {
                return responder.reply(Err(kit::error(error)));
            }
            if converted.losses.is_empty() && !password {
                return land_hwp(&docs, scope, path, converted.bytes, responder);
            }
            let message = hwp_loss_message_box(&converted.losses, password, docs.lang());
            let options = MessageBox {
                kind: Some(message.kind.into()),
                title: Some(message.title),
                message: message.message,
                detail: Some(message.detail),
                buttons: message.buttons.to_vec(),
                default_id: Some(message.default_id as usize),
                cancel_id: Some(message.cancel_id as usize),
                checkbox_label: None,
            };
            let checked = scope.clone();
            let completed = docs.clone();
            owned_save::message(
                &docs,
                scope,
                options,
                Box::new(move |answer| match answer {
                    Ok(0) => land_hwp(&completed, checked, path, converted.bytes, responder),
                    Ok(_) => responder.reply(Ok(canceled())),
                    Err(error) => responder.reply(Err(kit::error(error))),
                }),
            );
        },
    );
}
fn land_hwp(d: &Rc<Docs>, scope: Scope, path: String, bytes: Vec<u8>, responder: Responder) {
    let target = path.clone();
    let completed = d.clone();
    kit::on_worker(
        &d.services,
        move || stage(&target, bytes),
        move |staged| {
            let published = staged.and_then(|staged| scope.publish_staged(&path, staged));
            match published {
                Ok(published) => finish(&completed, scope, path, None, true, published, responder),
                Err(error) => responder.reply(Ok(hwp_failed(&path, &HwpError::Error(error)))),
            }
        },
    );
}

fn images_target(d: &Rc<Docs>, scope: Scope, directory: String, responder: Responder) {
    let view = scope.destination().view;
    let path = join(&[
        &d.temp_dir(),
        &format!("seuoffice-docs-images-{}.pdf", d.shell.random_uuid()),
    ]);
    let result = scope.effect(|| d.add_image_export(view, &path, &directory));
    responder.reply(
        result
            .map(|()| Obj::new().set("dir", directory).set("pdfPath", path).done())
            .map_err(kit::error),
    );
}
pub(super) fn pick_images(d: &Rc<Docs>, scope: Scope, responder: Responder) {
    use main_core::packages::electron_utils::dialog_memory::{
        FallbackDir, prepare_dialog_options, remember_open_path,
    };
    if let Some(directory) = &d.test_export_dir {
        return images_target(d, scope, directory.clone(), responder);
    }
    // A native save has no renderer origin: the document's own renderer
    // shows the dialog, and the operation's binding admits before it opens
    // and again before its answer is used.
    let origin = scope
        .origin()
        .cloned()
        .unwrap_or_else(|| scope.destination().clone());
    if let Err(error) = scope.effect(|| ()) {
        return responder.reply(Err(kit::error(error)));
    }
    let options = js_obj(vec![
        ("title", js_str(d.tm("dlgPickExportDir"))),
        (
            "properties",
            JsonValue::Array(vec![js_str("openDirectory"), js_str("createDirectory")]),
        ),
    ]);
    let fallback = d.default_save_dir().ok();
    let prepared = {
        let memory = d
            .memory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prepare_dialog_options(
            &memory,
            &options,
            fallback.as_deref().map(FallbackDir::Dir).as_ref(),
        )
    };
    let memory = d.memory.clone();
    let docs = d.clone();
    let completed = scope.clone();
    let done = kit::back_on_core(
        &d.services,
        move |picked: Result<Option<Vec<std::path::PathBuf>>, String>| {
            let directory = match picked {
                Ok(Some(paths)) => paths.into_iter().next(),
                Ok(None) => None,
                Err(error) => return responder.reply(Err(kit::error(error))),
            };
            let Some(directory) = directory else {
                return responder.reply(Ok(IpcValue::Null));
            };
            let directory = directory.to_string_lossy().into_owned();
            let result = completed.effect(|| {
                let mut memory = memory
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                remember_open_path(&mut memory, &directory, true);
            });
            if let Err(error) = result {
                return responder.reply(Err(kit::error(error)));
            }
            images_target(&docs, completed, directory, responder);
        },
    );
    d.dialogs.open_owned(
        scope.parent(),
        super::super::adapters::file_dialog(&prepared),
        origin,
        scope.destination().clone(),
        scope.cancel().clone(),
        scope.pending().clone(),
        Box::new(done),
    );
}
pub(super) fn take_pdf(d: &Rc<Docs>, scope: Scope, args: kit::Args, responder: Responder) {
    let view = scope.destination().view;
    let Some(path) = args.str(0).map(str::to_owned) else {
        return responder.reply(Ok(failed("not an image-export temp file")));
    };
    let taken = scope.effect(|| {
        d.image_temps
            .borrow_mut()
            .get_mut(&view)
            .is_some_and(|paths| paths.remove(&path))
    });
    match taken {
        Ok(true) => {}
        Ok(false) => return responder.reply(Ok(failed("not an image-export temp file"))),
        Err(error) => return responder.reply(Err(kit::error(error))),
    }
    let completed = scope.clone();
    kit::on_worker(
        &d.services,
        move || {
            let read = scope.check().and_then(|()| {
                std::fs::read(&path)
                    .map_err(|error| node_text(&super::super::fs_error(&error, "open", &path)))
            });
            // A native one-shot temp receipt permits cleanup even after its request expires.
            let _ = std::fs::remove_file(&path);
            read
        },
        move |read| {
            if let Err(error) = completed.check() {
                return responder.reply(Err(kit::error(error)));
            }
            responder.reply(Ok(match read {
                Ok(bytes) => Obj::new()
                    .set("ok", true)
                    .set("base64", base64(&bytes))
                    .done(),
                Err(error) => failed(error),
            }));
        },
    );
}
pub(super) fn image(d: &Rc<Docs>, scope: Scope, args: kit::Args, responder: Responder) {
    let view = scope.destination().view;
    let Some(directory) = args
        .str(0)
        .filter(|directory| {
            d.image_dirs.with(view, |allowed| {
                allowed.is_some_and(|allowed| allowed.contains(*directory))
            })
        })
        .map(str::to_owned)
    else {
        return responder.reply(Ok(failed("export target is not an authorized folder")));
    };
    let Some(name) = args
        .str(1)
        .filter(|name| basename(name) == *name && js_regex!(r"^[^/\\]+\.png$", "i").test(name))
        .map(str::to_owned)
    else {
        return responder.reply(Ok(failed("invalid image file name")));
    };
    let path = join(&[&directory, &name]);
    let target = path.clone();
    let bytes = buffer_from_base64(&js_string(args.get(2)));
    let worker = scope.clone();
    kit::on_worker(
        &d.services,
        move || write(&worker, &target, &bytes),
        move |result| {
            if let Err(error) = result {
                return responder.reply(Ok(failed(error)));
            }
            let landed = scope.target().unwrap_or(&path).to_owned();
            if let Err(error) = scope.note_landed(&landed) {
                return responder.reply(Err(kit::error(error)));
            }
            responder.reply(Ok(Obj::new().set("ok", true).set("path", path).done()));
        },
    );
}

#[cfg(test)]
mod tests;
