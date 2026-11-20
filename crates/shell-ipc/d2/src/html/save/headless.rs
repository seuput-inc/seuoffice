use super::*;
use crate::html::text_doc::HeadlessPublication;

impl Exporter {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn export_invocation(
        self: &Rc<Self>,
        view: ViewId,
        format: &str,
        html: String,
        doc_path: Option<String>,
        target: String,
        publication: HeadlessPublication,
        done: Reply,
    ) {
        let html = if format == "docx" {
            match self.plan(view, &html, MarkedFormat::Docx, false) {
                Ok(Some(html)) => html,
                Ok(None) => return done(failure(marked_export_refusal(&self.lang()))),
                Err(error) => return done(failure(error)),
            }
        } else {
            html
        };
        let engine = self.engine.clone();
        let conversion = publication.clone();
        let is_pdf = format == "pdf";
        let slot = Rc::new(std::cell::RefCell::new(Some(done)));
        let expired = slot.clone();
        let pending = publication.pending.clone();
        let watchdog = if is_pdf {
            None
        } else {
            Some(kit::after(&self.services, HTML_EXPORT_TIMEOUT, move || {
                pending.cancel();
                if let Some(done) = expired.borrow_mut().take() {
                    done(failure("html export timed out"));
                }
            }))
        };
        let services = self.services.clone();
        let output = target.clone();
        kit::on_worker(
            &self.services,
            move || {
                if is_pdf {
                    engine.print_pdf_invocation(
                        &conversion.document,
                        &conversion.owner,
                        &html,
                        doc_path.as_deref(),
                        &conversion.cancel,
                        &conversion.pending,
                    )
                } else {
                    let work = main_core::node_fs::mkdtemp_random(&join(&[
                        &std::env::temp_dir().to_string_lossy(),
                        "seuoffice-html-docx-",
                    ]))
                    .map_err(|error| error.message)?;
                    let result =
                        Self::preview_file(&html, doc_path.as_deref(), &work, "export.html")
                            .and_then(|path| {
                                engine.html_to_docx_invocation(
                                    &conversion.document,
                                    &conversion.owner,
                                    &path,
                                    &conversion.cancel,
                                    &conversion.pending,
                                )
                            });
                    let _ = std::fs::remove_dir_all(&work);
                    result
                }
            },
            move |result| {
                if let Some(timer) = watchdog {
                    kit::clear(timer);
                }
                let bytes = match result {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        if let Some(done) = slot.borrow_mut().take() {
                            done(failure(error));
                        }
                        return;
                    }
                };
                if slot.borrow().is_none() {
                    return;
                }
                let admission = publication.admission();
                kit::on_worker(
                    &services,
                    move || {
                        main_core::packages::electron_utils::atomic_write::owned::write_file(
                            &target, &bytes, admission,
                        )
                        .map_err(|error| error.message)
                    },
                    move |written| {
                        if let Some(done) = slot.borrow_mut().take() {
                            done(match written {
                                Ok(()) => saved_at(&output),
                                Err(error) => failure(error),
                            });
                        }
                    },
                );
            },
        );
    }
}
