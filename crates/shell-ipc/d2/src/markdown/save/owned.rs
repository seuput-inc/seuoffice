mod images;
pub(super) use images::{Image, dispose_images};

use super::*;
use crate::html::text_doc::OwnedSave;
use shell_contract::ipc::{CallContext, NativeContext};
use shell_contract::runtime::CancelToken;

#[derive(Clone)]
pub(super) struct Approved {
    pub format: &'static str,
    pub path: Option<String>,
    pub owner: OwnedSave,
}

impl Md {
    fn approved_owned(
        &self,
        view: ViewId,
        format: &str,
        requested: &IpcValue,
        native: Option<&NativeContext>,
    ) -> Result<Approved, String> {
        let approved = self
            .owned_approvals
            .get(view)
            .ok_or("markdown: save target not approved")?;
        if approved.format != format || !approved.owner.matches(native) {
            return Err("markdown: save target not approved".into());
        }
        let matches = match &approved.path {
            Some(path) => kit::as_str(requested) == Some(path.as_str()),
            None => matches!(requested, IpcValue::Undefined),
        };
        if !matches {
            return Err("markdown: save target not approved".into());
        }
        approved.owner.check_document(&self.e.docs, view)?;
        approved.owner.effect(&self.e.docs, || {
            self.owned_approvals.remove(view);
            Ok(())
        })?;
        Ok(approved)
    }

    pub(super) fn finish_owned(&self, view: ViewId, result: IpcValue) {
        let waiter = self.e.docs.take_save_waiter(view);
        if let Some(waiter) = waiter {
            waiter(saved_ok(&result));
        }
    }

    pub(super) fn export_owned_followup(
        self: &Rc<Self>,
        cx: &CallContext,
        format: &'static str,
        input: &IpcValue,
        responder: Responder,
    ) {
        let view = cx.sender.view;
        let approved = match self.approved_owned(
            view,
            format,
            field(input, "saveAsTarget"),
            cx.native.as_ref(),
        ) {
            Err(error) => return responder.reply(Ok(failure(error))),
            Ok(approved) => approved,
        };
        let cancel = approved.owner.cancel();
        let forwarding = cx.cancel.on_cancel(move || cancel.cancel());
        let me = Rc::clone(self);
        let done: Exported = Box::new(move |result| {
            let _forwarding = forwarding;
            me.finish_owned(view, result.clone());
            responder.reply(Ok(result));
        });
        let name = export_name(field(input, "suggestedName"), &self.untitled());
        let request = input.clone();
        let me = Rc::clone(self);
        let owner = approved.owner.clone();
        self.export_path_owned(
            view,
            approved,
            name,
            Box::new(move |target| {
                let target = match target {
                    Err(error) => return done(failure(error)),
                    Ok(None) => return done(canceled()),
                    Ok(Some(target)) => target,
                };
                if format == "pdf" {
                    let html = match field(&request, "html") {
                        IpcValue::String(html) if !html.is_empty() => html.clone(),
                        _ => return done(failure("markdown: bad export request")),
                    };
                    return me.write_pdf_owned(view, html, target, owner, done);
                }
                let bytes = match field(&request, "base64") {
                    IpcValue::String(base64) if !base64.is_empty() => {
                        jscompat::buffer_from_base64(base64)
                    }
                    _ => return done(failure("markdown: bad export request")),
                };
                if format == "docx" {
                    return me.publish_owned(view, target, bytes, owner, done);
                }
                if let Err(error) = owner.check_document(&me.e.docs, view) {
                    return done(failure(error));
                }
                let engine = me.engine.clone();
                let converted = Rc::clone(&me);
                let conversion_cancel = owner.cancel();
                kit::on_worker(
                    &me.services,
                    move || engine.docx_to_odt_cancellable(bytes, &conversion_cancel),
                    move |conversion| {
                        let OdtConversion { odt, report } = match conversion {
                            Err(error) => return done(failure(error)),
                            Ok(conversion) => conversion,
                        };
                        if let Err(error) = owner.check_document(&converted.e.docs, view) {
                            return done(failure(error));
                        }
                        if report.is_empty() {
                            return converted.publish_owned(view, target, odt, owner, done);
                        }
                        let options = odt_report_box(&report, &converted.lang());
                        let retained = owner.clone();
                        let finish = Rc::clone(&converted);
                        converted
                            .e
                            .docs
                            .message_owned(view, options, owner, move |answer| match answer {
                                Err(error) => done(failure(error)),
                                Ok(answer) if answer.response != 0 => done(canceled()),
                                Ok(_) => finish.publish_owned(view, target, odt, retained, done),
                            });
                    },
                );
            }),
        );
    }

    fn export_path_owned(
        self: &Rc<Self>,
        view: ViewId,
        approved: Approved,
        name: String,
        done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    ) {
        if let Some(path) = approved.path {
            return done(Ok(Some(path)));
        }
        let (label, extensions) = match approved.format {
            "docx" => ("Word".to_owned(), vec!["docx"]),
            "pdf" => ("PDF".to_owned(), vec!["pdf"]),
            _ => (odt_filter_name(&self.lang()), vec!["odt"]),
        };
        let fallback = match approved.owner.default_directory() {
            Some(path) => path.to_owned(),
            None => {
                let owner = approved.owner.clone();
                let me = Rc::clone(self);
                return self
                    .e
                    .docs
                    .prepare_default_directory_owned(owner, move |prepared| match prepared {
                        Err(error) => done(Err(error)),
                        Ok(()) => me.export_path_owned(view, approved, name, done),
                    });
            }
        };
        let options = dialog_options(
            None,
            Some(join(&[&fallback, &format!("{name}.{}", approved.format)])),
            &[(label, extensions)],
            &[],
        );
        self.e
            .docs
            .save_dialog_owned(view, options, approved.owner, done);
    }

    fn publish_owned(
        self: &Rc<Self>,
        view: ViewId,
        target: String,
        bytes: Vec<u8>,
        owner: OwnedSave,
        done: Exported,
    ) {
        let output = target.clone();
        self.e
            .docs
            .publish_export_owned(view, target, bytes, owner, move |published| {
                done(match published {
                    Err(error) => failure(error),
                    Ok(_) => saved_at(&output),
                })
            });
    }

    pub(super) fn write_pdf_owned(
        self: &Rc<Self>,
        view: ViewId,
        html: String,
        target: String,
        owner: OwnedSave,
        done: Exported,
    ) {
        let me = Rc::clone(self);
        let retained = owner.clone();
        self.print_owned(
            view,
            html,
            owner,
            Box::new(move |printed| {
                let bytes = match printed {
                    Err(error) => return done(failure(error)),
                    Ok(bytes) => bytes,
                };
                let finished = Rc::clone(&me);
                let final_owner = retained.clone();
                let output = target.clone();
                me.e.docs
                    .publish_export_owned(view, target, bytes, retained, move |published| {
                        let published = match published {
                            Err(error) => return done(failure(error)),
                            Ok(published) => published,
                        };
                        if finished.e.docs.is_headless() {
                            return done(saved_at(&output));
                        }
                        let shell = finished.shell.clone();
                        let prepare: crate::html::text_doc::Prepare = Box::new(
                            move |output,
                                  input,
                                  participants,
                                  cancel,
                                  pending,
                                  additional,
                                  effects,
                                  accepted,
                                  wake| {
                                shell.prepare_generated_output(
                                    output,
                                    input,
                                    "pdf",
                                    participants,
                                    cancel,
                                    pending,
                                    additional,
                                    effects,
                                    accepted,
                                    wake,
                                )
                            },
                        );
                        finished.e.docs.complete_export(
                            final_owner,
                            published,
                            finished.os.clone(),
                            prepare,
                            Box::new(move |opened| {
                                done(match opened {
                                    Err(error) => failure(error),
                                    Ok(()) => saved_at(&output),
                                })
                            }),
                        );
                    });
            }),
        );
    }

    pub(super) fn write_pdf_invocation(
        self: &Rc<Self>,
        html: String,
        target: String,
        publication: crate::html::text_doc::HeadlessPublication,
        done: Exported,
    ) {
        let engine = self.engine.clone();
        let services = self.services.clone();
        let temp = self.temp();
        let printing = publication.clone();
        let finish_services = services.clone();
        kit::on_worker(
            &services,
            move || {
                let window = engine
                    .print_window_invocation(
                        &printing.document,
                        &printing.owner,
                        &printing.cancel,
                        &printing.pending,
                    )
                    .map_err(JsError::plain);
                begin_print_markdown_pdf(
                    || window.map(Window),
                    &html,
                    &MarkdownPrintOptions::default(),
                    &temp.to_string_lossy(),
                )
            },
            move |step| {
                let write_services = finish_services.clone();
                drive_owned_print(
                    &finish_services,
                    step,
                    Box::new(move |printed| {
                        let bytes = match printed {
                            Ok(bytes) => bytes,
                            Err(error) => return done(failure(error)),
                        };
                        let output = target.clone();
                        let admission = publication.admission();
                        kit::on_worker(
                            &write_services,
                            move || {
                                main_core::packages::electron_utils::atomic_write::owned::write_file(&target,&bytes,admission).map_err(|error|error.message)
                            },
                            move |written| {
                                done(match written {
                                    Ok(()) => saved_at(&output),
                                    Err(error) => failure(error),
                                })
                            },
                        );
                    }),
                );
            },
        );
    }

    fn print_owned(
        self: &Rc<Self>,
        view: ViewId,
        html: String,
        owner: OwnedSave,
        done: Box<dyn FnOnce(Result<Vec<u8>, String>)>,
    ) {
        if let Err(error) = owner.check_document(&self.e.docs, view) {
            return done(Err(error));
        }
        let engine = self.engine.clone();
        let services = self.services.clone();
        let temp = self.temp();
        kit::on_worker(
            &self.services,
            move || {
                let window = match owner.authority() {
                    crate::html::text_doc::SaveAuthority::Renderer(origin) => engine
                        .print_window_owned(
                            origin,
                            owner.destination(),
                            &owner.cancel(),
                            &owner.pending(),
                        ),
                    crate::html::text_doc::SaveAuthority::Native(binding) => engine
                        .print_window_native(
                            binding,
                            &owner.captured_participants(),
                            &owner.cancel(),
                            &owner.pending(),
                            &owner.callback_cancels(),
                        ),
                }
                .map_err(JsError::plain);
                begin_print_markdown_pdf(
                    || window.map(Window),
                    &html,
                    &MarkdownPrintOptions::default(),
                    &temp.to_string_lossy(),
                )
            },
            move |step| drive_owned_print(&services, step, done),
        );
    }

    pub(super) fn confirm_owned(
        self: &Rc<Self>,
        cx: &CallContext,
        format: &str,
        responder: Responder,
    ) {
        let view = cx.sender.view;
        let Some(approved) = self.owned_approvals.get(view) else {
            return responder.reply(Ok("cancel".into_ipc()));
        };
        if approved.format != format || !approved.owner.matches(cx.native.as_ref()) {
            return responder.reply(Ok("cancel".into_ipc()));
        }
        if self.e.docs.is_headless() {
            self.owned_approvals.remove(view);
            self.finish_owned(view, canceled());
            return responder.reply(Ok("cancel".into_ipc()));
        }
        let options = self.marked_export_options(format);
        let owner = approved.owner.clone();
        let me = Rc::clone(self);
        let cancel = owner.cancel();
        let forwarding = cx.cancel.on_cancel(move || cancel.cancel());
        self.e
            .docs
            .message_owned(view, options, owner.clone(), move |answer| {
                let _forwarding = forwarding;
                let choice = match answer {
                    Err(error) => {
                        me.owned_approvals.remove(view);
                        me.finish_owned(view, failure(error));
                        return responder.reply(Ok("cancel".into_ipc()));
                    }
                    Ok(answer) => marked_odt_choice(answer.response as f64, true),
                };
                let reply = match choice {
                    MarkedOdtChoice::Cancel => {
                        me.owned_approvals.remove(view);
                        me.finish_owned(view, canceled());
                        "cancel"
                    }
                    MarkedOdtChoice::Docx => {
                        let mut redirected = approved;
                        redirected.format = "docx";
                        redirected.path = None;
                        if let Err(error) = owner.effect(&me.e.docs, || {
                            me.owned_approvals.insert(view, redirected);
                            Ok(())
                        }) {
                            me.finish_owned(view, failure(error));
                            return responder.reply(Ok("cancel".into_ipc()));
                        }
                        "docx"
                    }
                    MarkedOdtChoice::Clear => "clear",
                };
                responder.reply(Ok(reply.into_ipc()));
            });
    }
}

fn drive_owned_print(
    services: &Services,
    step: PdfStep<Window>,
    done: Box<dyn FnOnce(Result<Vec<u8>, String>)>,
) {
    let (call, mut job) = match step {
        WindowStep::Done(result) => return done(result.map_err(|error| error.message)),
        WindowStep::Await { call, job } => (call, job),
    };
    let waiting: Rc<RefCell<Option<PdfJob<Window>>>> = Rc::default();
    let next = services.clone();
    let holder = waiting.clone();
    let answer = kit::back_on_core(services, move |answer| {
        if let Some(job) = holder.borrow_mut().take() {
            let resumed = next.clone();
            kit::on_worker(
                &next,
                move || job.resume(answer),
                move |step| drive_owned_print(&resumed, step, done),
            );
        }
    });
    job.window().0.call(call, answer);
    *waiting.borrow_mut() = Some(job);
}
