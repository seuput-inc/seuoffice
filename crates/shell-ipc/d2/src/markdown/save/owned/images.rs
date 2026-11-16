use super::*;
mod session;
use session::SessionWorker;
use shell_contract::runtime::CancelSubscription;
use std::rc::Weak;

thread_local! {
    static NEXT_CLEANUP:std::cell::Cell<u64>=const{std::cell::Cell::new(1)};
    static CLEANUP:RefCell<std::collections::HashMap<u64,(Weak<Md>,String)>>=RefCell::new(std::collections::HashMap::new());
}

#[derive(Clone)]
pub(crate) struct Image {
    view: ViewId,
    id: String,
    owner: OwnedSave,
    sessions: Arc<SessionWorker>,
    cleanup: u64,
    _cancel: Arc<CancelSubscription>,
}

impl Md {
    pub(in super::super) fn owns_image(&self, id: &str) -> bool {
        self.owned_images.borrow().contains_key(id)
    }

    pub(in super::super) fn prepare_images_owned(
        self: &Rc<Self>,
        cx: &CallContext,
        input: &IpcValue,
        responder: Responder,
    ) {
        let view = cx.sender.view;
        let approved = match self.approved_owned(
            view,
            "png",
            field(input, "saveAsTarget"),
            cx.native.as_ref(),
        ) {
            Err(error) => return responder.reply(Ok(failure(error))),
            Ok(approved) => approved,
        };
        let html = match field(input, "html") {
            IpcValue::String(html) if !html.is_empty() => html.clone(),
            _ => {
                let result = failure("Empty document");
                self.finish_owned(view, result.clone());
                return responder.reply(Ok(result));
            }
        };
        let Some(target) = approved.path else {
            let result = failure("markdown: save target not approved");
            self.finish_owned(view, result.clone());
            return responder.reply(Ok(result));
        };
        let owner = approved.owner;
        let cancel = owner.cancel();
        let forwarding = cx.cancel.on_cancel(move || cancel.cancel());
        let worker = owner.clone();
        let shell = self.shell.clone();
        let sessions = Arc::new(SessionWorker::new());
        let worker_sessions = sessions.clone();
        let me = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                let uuid = || shell.random_uuid();
                let suffix = || {
                    uuid()
                        .chars()
                        .filter(char::is_ascii_alphanumeric)
                        .take(6)
                        .collect()
                };
                worker_sessions
                    .run(|sessions| {
                        sessions.start_files_authorized(
                            &ImageExportEnv {
                                random_uuid: &uuid,
                                mkdtemp_suffix: &suffix,
                            },
                            f64::from(view.0),
                            &target,
                            &mut |effect| {
                                crate::html::text_doc::owned_write::admit(&worker, effect)
                            },
                        )
                    })
                    .map_err(|error| error.message)
            },
            move |started| {
                let id = match started {
                    Err(error) => {
                        let result = failure(error);
                        me.finish_owned(view, result.clone());
                        return responder.reply(Ok(result));
                    }
                    Ok(id) => id,
                };
                let cleanup = NEXT_CLEANUP.with(|next| {
                    let key = next.get();
                    next.set(
                        key.checked_add(1)
                            .expect("image cleanup identities exhausted"),
                    );
                    key
                });
                let core = me.services.core.clone();
                let subscription = owner.cancel().on_cancel(move || {
                    core.post(Box::new(move || {
                        if let Some((me, id)) =
                            CLEANUP.with(|entries| entries.borrow_mut().remove(&cleanup))
                        {
                            if let Some(me) = me.upgrade() {
                                me.dispose_image(&id);
                            }
                        }
                    }))
                });
                let image = Image {
                    view,
                    id: id.clone(),
                    owner: owner.clone(),
                    sessions,
                    cleanup,
                    _cancel: Arc::new(subscription),
                };
                if let Err(error) = owner.effect(&me.e.docs, || {
                    CLEANUP.with(|entries| {
                        entries
                            .borrow_mut()
                            .insert(cleanup, (Rc::downgrade(&me), id.clone()));
                    });
                    me.owned_images.borrow_mut().insert(id.clone(), image);
                    Ok(())
                }) {
                    let result = failure(error);
                    me.finish_owned(view, result.clone());
                    return responder.reply(Ok(result));
                }
                let finished = Rc::clone(&me);
                let retained = owner.clone();
                me.print_owned(
                    view,
                    html,
                    owner,
                    Box::new(move |printed| {
                        let _forwarding = forwarding;
                        let printed = printed.and_then(|pdf| {
                            retained
                                .check_document(&finished.e.docs, view)
                                .map(|()| pdf)
                        });
                        match printed {
                            Ok(pdf) => responder.reply(Ok(obj([
                                ("ok", true.into_ipc()),
                                ("id", id.into_ipc()),
                                ("pdfBase64", jscompat::base64(&pdf).into_ipc()),
                            ]))),
                            Err(error) => {
                                let result = failure(error);
                                finished.finish_owned(view, result.clone());
                                finished.dispose_image(&id);
                                responder.reply(Ok(result));
                            }
                        }
                    }),
                );
            },
        );
    }

    fn image_owned(&self, cx: &CallContext, id: &str) -> Result<Image, String> {
        let image = self
            .owned_images
            .borrow()
            .get(id)
            .cloned()
            .ok_or("Export session is not authorized")?;
        if image.view != cx.sender.view || !image.owner.matches(cx.native.as_ref()) {
            return Err("Export session is not authorized".into());
        }
        image.owner.check_document(&self.e.docs, image.view)?;
        Ok(image)
    }

    pub(in super::super) fn write_image_owned(
        self: &Rc<Self>,
        cx: &CallContext,
        id: &str,
        page: f64,
        base64: Option<&str>,
        responder: Responder,
    ) {
        let image = match self.image_owned(cx, id) {
            Err(error) => return responder.reply(Ok(failure(error))),
            Ok(image) => image,
        };
        let base64 = base64.map(str::to_owned);
        let cancel = image.owner.cancel();
        let forwarding = cx.cancel.on_cancel(move || cancel.cancel());
        kit::on_worker(
            &self.services,
            move || {
                image
                    .sessions
                    .run(|sessions| {
                        sessions.write_authorized(
                            f64::from(image.view.0),
                            &image.id,
                            page,
                            base64.as_deref(),
                            &mut |effect| {
                                crate::html::text_doc::owned_write::admit(&image.owner, effect)
                            },
                        )
                    })
                    .map_err(|error| error.message)
            },
            move |written| {
                let _forwarding = forwarding;
                responder.reply(Ok(match written {
                    Err(error) => failure(error),
                    Ok(()) => obj([("ok", true.into_ipc())]),
                }));
            },
        );
    }

    pub(in super::super) fn finish_image_owned(
        self: &Rc<Self>,
        cx: &CallContext,
        id: &str,
        success: bool,
        responder: Responder,
    ) {
        let image = match self.image_owned(cx, id) {
            Err(error) => return responder.reply(Ok(failure(error))),
            Ok(image) => image,
        };
        if let Err(error) = image.owner.effect(&self.e.docs, || {
            self.owned_images.borrow_mut().remove(id);
            CLEANUP.with(|entries| {
                entries.borrow_mut().remove(&image.cleanup);
            });
            Ok(())
        }) {
            return responder.reply(Ok(failure(error)));
        }
        let owner = image.owner.clone();
        let view = image.view;
        let me = Rc::clone(self);
        let cancel = owner.cancel();
        let forwarding = cx.cancel.on_cancel(move || cancel.cancel());
        kit::on_worker(
            &self.services,
            move || {
                image
                    .sessions
                    .run(|sessions| {
                        sessions.finish_export_authorized(
                            f64::from(view.0),
                            &image.id,
                            success,
                            &mut |effect| {
                                crate::html::text_doc::owned_write::admit(&image.owner, effect)
                            },
                        )
                    })
                    .map_err(|error| error.message)
            },
            move |finished| {
                let completing = Rc::clone(&me);
                let done: Exported = Box::new(move |result| {
                    let _forwarding = forwarding;
                    completing.finish_owned(view, result.clone());
                    responder.reply(Ok(result));
                });
                let finished = match finished {
                    Err(error) => return done(failure(error)),
                    Ok(None) => return done(canceled()),
                    Ok(Some(finished)) => finished,
                };
                let dir = finished.dir;
                if me.e.docs.is_headless() {
                    return done(saved_at(&dir));
                }
                me.shell.export_written_owned(
                    owner.origin().clone(),
                    owner.destination().clone(),
                    owner.cancel(),
                    owner.pending(),
                    owner.effects().clone(),
                    finished.first.into(),
                    "reveal",
                    Box::new(move |revealed| {
                        done(match revealed {
                            Err(error) => failure(error),
                            Ok(()) => saved_at(&dir),
                        })
                    }),
                );
            },
        );
    }

    fn dispose_image(self: &Rc<Self>, id: &str) {
        let Some(image) = self.owned_images.borrow_mut().remove(id) else {
            return;
        };
        CLEANUP.with(|entries| {
            entries.borrow_mut().remove(&image.cleanup);
        });
        kit::on_worker(&self.services, move || drop(image), |()| {});
    }
}

pub(crate) fn dispose_images(
    services: &Services,
    images: &Rc<RefCell<std::collections::HashMap<String, Image>>>,
    view: ViewId,
) {
    let ids = images
        .borrow()
        .iter()
        .filter(|(_, image)| image.view == view)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        let Some(image) = images.borrow_mut().remove(&id) else {
            continue;
        };
        CLEANUP.with(|entries| {
            entries.borrow_mut().remove(&image.cleanup);
        });
        kit::on_worker(services, move || drop(image), |()| {});
    }
}
