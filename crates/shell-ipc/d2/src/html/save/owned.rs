use super::*;

impl Exporter {
    pub(super) fn export_owned(
        self: &Rc<Self>,
        view: ViewId,
        format: &'static str,
        html: String,
        doc_path: Option<String>,
        target: String,
        request: OwnedSave,
        done: Reply,
    ) {
        if let Err(error) = request.check_document(&self.e.docs, view) {
            return done(failure(error));
        }
        if format == "pdf" {
            let engine = self.engine.clone();
            let worker = request.clone();
            let me = Rc::clone(self);
            return kit::on_worker(
                &self.services,
                move || match worker.authority() {
                    crate::html::text_doc::SaveAuthority::Renderer(origin) => engine
                        .print_pdf_owned(
                            origin,
                            worker.destination(),
                            &html,
                            doc_path.as_deref(),
                            &worker.cancel(),
                            &worker.pending(),
                        ),
                    crate::html::text_doc::SaveAuthority::Native(binding) => engine
                        .print_pdf_native(
                            binding,
                            &worker.captured_participants(),
                            &html,
                            doc_path.as_deref(),
                            &worker.cancel(),
                            &worker.pending(),
                            &worker.callback_cancels(),
                        ),
                },
                move |printed| match printed {
                    Err(error) => done(failure(error)),
                    Ok(bytes) => me.publish_owned(view, format, target, bytes, request, done),
                },
            );
        }
        let marked = if format == "docx" {
            MarkedFormat::Docx
        } else {
            MarkedFormat::Odt
        };
        let me = Rc::clone(self);
        self.plan_owned(
            view,
            html,
            marked,
            request.clone(),
            Box::new(move |planned| {
                let html = match planned {
                    Err(error) => return done(failure(error)),
                    Ok(None) => return done(canceled()),
                    Ok(Some(html)) => html,
                };
                if format == "docx" {
                    let render = Rc::clone(&me);
                    let checked = request.clone();
                    let output = target.clone();
                    return me.prepare_export_owned(
                        target.into(),
                        &request,
                        Box::new(move |prepared| {
                            match prepared {
                                Err(error) => return done(failure(error)),
                                Ok(false) => return done(canceled()),
                                Ok(true) => {}
                            }
                            if let Err(error) = checked.check_document(&render.e.docs, view) {
                                return done(failure(error));
                            }
                            let finish = Rc::clone(&render);
                            let final_owner = checked.clone();
                            render.render_docx_owned(
                                view,
                                html,
                                doc_path,
                                checked,
                                Box::new(move |rendered| match rendered {
                                    Err(error) => done(failure(error)),
                                    Ok(bytes) => finish.publish_owned(
                                        view,
                                        "docx",
                                        output,
                                        bytes,
                                        final_owner,
                                        done,
                                    ),
                                }),
                            );
                        }),
                    );
                }
                let converted = Rc::clone(&me);
                let checked = request.clone();
                me.render_docx_owned(
                    view,
                    html,
                    doc_path,
                    request,
                    Box::new(move |rendered| {
                        let bytes = match rendered {
                            Err(error) => return done(failure(error)),
                            Ok(bytes) => bytes,
                        };
                        if let Err(error) = checked.check_document(&converted.e.docs, view) {
                            return done(failure(error));
                        }
                        let engine = converted.engine.clone();
                        let finish = Rc::clone(&converted);
                        let (conversion_cancel, subscriptions) = checked.worker_cancel();
                        kit::on_worker(
                            &converted.services,
                            move || {
                                let _subscriptions = subscriptions;
                                engine.docx_to_odt_cancellable(bytes, &conversion_cancel)
                            },
                            move |conversion| {
                                let OdtConversion { odt, report } = match conversion {
                                    Err(error) => return done(failure(error)),
                                    Ok(converted) => converted,
                                };
                                if let Err(error) = checked.check_document(&finish.e.docs, view) {
                                    return done(failure(error));
                                }
                                if report.is_empty() {
                                    return finish
                                        .prepare_odt_owned(view, target, odt, checked, done);
                                }
                                let options = odt_report_box(&report, &finish.lang());
                                let after = Rc::clone(&finish);
                                let retained = checked.clone();
                                finish.e.docs.message_owned(
                                    view,
                                    options,
                                    checked,
                                    move |answer| match answer {
                                        Err(error) => done(failure(error)),
                                        Ok(answer) if answer.response != 0 => done(canceled()),
                                        Ok(_) => after
                                            .prepare_odt_owned(view, target, odt, retained, done),
                                    },
                                );
                            },
                        );
                    }),
                );
            }),
        );
    }

    fn plan_owned(
        self: &Rc<Self>,
        view: ViewId,
        html: String,
        format: MarkedFormat,
        request: OwnedSave,
        done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    ) {
        match self.plan(view, &html, format, false) {
            Err(error) => done(Err(error)),
            Ok(Some(html)) => done(Ok(Some(html))),
            Ok(None) => {
                if self.e.docs.is_headless() {
                    return done(Err(marked_export_refusal(&self.lang())));
                }
                self.e.docs.message_owned(
                    view,
                    marked_export_box(format, &self.lang()),
                    request,
                    move |answer| {
                        let answer = match answer {
                            Err(error) => return done(Err(error)),
                            Ok(answer) => answer,
                        };
                        let choice = if marked_export_clears(answer.response) {
                            MarkedExportChoice::Clear
                        } else {
                            MarkedExportChoice::Cancel
                        };
                        done(
                            match plan_marked_export::<PlanError>(&html, Some(|| Ok(choice))) {
                                Ok(MarkedExportPlan::Write { html }) => Ok(Some(html)),
                                Ok(MarkedExportPlan::Declined) => Ok(None),
                                Err(PlanError) => {
                                    Err("the hidden marks could not be cleared".into())
                                }
                            },
                        );
                    },
                );
            }
        }
    }

    fn render_docx_owned(
        self: &Rc<Self>,
        view: ViewId,
        html: String,
        doc_path: Option<String>,
        request: OwnedSave,
        done: Box<dyn FnOnce(Result<Vec<u8>, String>)>,
    ) {
        if let Err(error) = request.check_document(&self.e.docs, view) {
            return done(Err(error));
        }
        let (cancel, subscriptions) = request.worker_cancel();
        let timeout_cancel = cancel.clone();
        let expired = Rc::new(std::cell::Cell::new(false));
        let timeout_expired = expired.clone();
        let watchdog = kit::after(&self.services, HTML_EXPORT_TIMEOUT, move || {
            timeout_expired.set(true);
            timeout_cancel.cancel();
        });
        let engine = self.engine.clone();
        let docs = Rc::clone(&self.e.docs);
        let checked = request.clone();
        kit::on_worker(
            &self.services,
            move || {
                let _subscriptions = subscriptions;
                if cancel.is_cancelled() {
                    return Err("The export was cancelled".into());
                }
                let work = main_core::node_fs::mkdtemp_random(&join(&[
                    &std::env::temp_dir().to_string_lossy(),
                    "seuoffice-html-docx-",
                ]))
                .map_err(|error| error.message)?;
                let result = Self::preview_file(&html, doc_path.as_deref(), &work, "document.html")
                    .and_then(|path| match request.authority() {
                        crate::html::text_doc::SaveAuthority::Renderer(origin) => engine
                            .html_to_docx_owned(
                                origin,
                                request.destination(),
                                &path,
                                &cancel,
                                &request.pending(),
                            ),
                        crate::html::text_doc::SaveAuthority::Native(binding) => {
                            let mut additional = request.callback_cancels();
                            additional.push(cancel.clone());
                            engine.html_to_docx_native(
                                binding,
                                &request.captured_participants(),
                                &path,
                                &request.cancel(),
                                &request.pending(),
                                &additional,
                            )
                        }
                    });
                let _ = std::fs::remove_dir_all(&work);
                result
            },
            move |result| {
                kit::clear(watchdog);
                if expired.get() {
                    done(Err("html export timed out".into()))
                } else {
                    done(
                        result
                            .and_then(|bytes| checked.check_document(&docs, view).map(|()| bytes)),
                    )
                }
            },
        );
    }

    fn prepare_odt_owned(
        self: &Rc<Self>,
        view: ViewId,
        target: String,
        bytes: Vec<u8>,
        request: OwnedSave,
        done: Reply,
    ) {
        let me = Rc::clone(self);
        let checked = request.clone();
        let path = target.clone();
        self.prepare_export_owned(
            target.into(),
            &request,
            Box::new(move |prepared| match prepared {
                Err(error) => done(failure(error)),
                Ok(false) => done(canceled()),
                Ok(true) => me.publish_owned(view, "odt", path, bytes, checked, done),
            }),
        );
    }

    fn prepare_export_owned(
        &self,
        path: PathBuf,
        request: &OwnedSave,
        then: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        if let Err(error) = request.check_target(&path) {
            return then(Err(error));
        }
        match request.authority() {
            crate::html::text_doc::SaveAuthority::Renderer(origin) => {
                self.e.shell.docx_export_prepare_owned(
                    origin.clone(),
                    request.destination().clone(),
                    request.cancel(),
                    request.pending(),
                    request.effects().clone(),
                    path,
                    then,
                )
            }
            crate::html::text_doc::SaveAuthority::Native(_) => match request.native_destination() {
                Ok(Some(selection)) => self.e.shell.docx_export_prepare_native(
                    selection,
                    request.captured_participants(),
                    request.cancel(),
                    request.pending(),
                    request.callback_cancels(),
                    request.effects().clone(),
                    then,
                ),
                Ok(None) => then(Err("native export destination was not selected".into())),
                Err(error) => then(Err(error)),
            },
        }
    }

    fn publish_owned(
        self: &Rc<Self>,
        view: ViewId,
        format: &'static str,
        target: String,
        bytes: Vec<u8>,
        request: OwnedSave,
        done: Reply,
    ) {
        let me = Rc::clone(self);
        let retained = request.clone();
        let output = target.clone();
        self.e
            .docs
            .publish_export_owned(view, target, bytes, request, move |published| {
                let published = match published {
                    Err(error) => return done(failure(error)),
                    Ok(published) => published,
                };
                if me.e.docs.is_headless() {
                    return done(saved_at(&output));
                }
                let shell = me.e.shell.clone();
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
                            format,
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
                me.e.docs.complete_export(
                    retained,
                    published,
                    me.os.clone(),
                    prepare,
                    Box::new(move |opened| match opened {
                        Err(error) => done(failure(error)),
                        Ok(()) => done(saved_at(&output)),
                    }),
                );
            });
    }
}
