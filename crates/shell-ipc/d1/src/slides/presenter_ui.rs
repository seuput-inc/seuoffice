use super::super::engine::{PresenterWindowAction, RenderOwner};
use super::*;
use shell_contract::ipc::Responder;
use shell_contract::runtime::CancelToken;

fn same_renderer(a: &RenderOwner, b: &RenderOwner) -> bool {
    a.view == b.view
        && a.context
            .renderer()
            .zip(b.context.renderer())
            .is_some_and(|(a, b)| a.same_generation(b))
}

pub(in crate::slides) fn presenter_swap(
    app: &App,
    cx: &CallContext,
    _args: Args,
    responder: Responder,
) {
    // `!show || !aWin` answers false whatever the document is
    let has_audience = app
        .state
        .borrow()
        .presenter_shows
        .get(&cx.sender.view)
        .is_some_and(|show| show.audience.is_some() && show.audience_owner.is_some());
    if !has_audience {
        return responder.reply(Ok(IpcValue::Bool(false)));
    }
    let source = match super::super::session::render_owner(cx)
        .and_then(|source| app.capture_save_owner(cx.sender.view, source))
    {
        Ok(source) => source,
        Err(error) => return responder.reply(Err(error)),
    };
    let audience = {
        let state = app.state.borrow();
        let Some(show) = state.presenter_shows.get(&source.view) else {
            return responder.reply(Ok(IpcValue::Bool(false)));
        };
        if !same_renderer(&show.owner, &source) {
            return responder.reply(Err(thrown("The presenter document was replaced")));
        }
        show.audience.zip(show.audience_owner.clone())
    };
    let Some(((audience_view, audience_window), destination)) = audience else {
        return responder.reply(Ok(IpcValue::Bool(false)));
    };
    let Some(presenter) = app.shell.host_window_for(source.view) else {
        return responder.reply(Ok(IpcValue::Bool(false)));
    };
    if destination.view != audience_view {
        return responder.reply(Err(thrown("The audience binding was replaced")));
    }
    let presenter_display = match app.shell.display_of(Some(presenter)) {
        Ok(display) => display,
        Err(error) => return responder.reply(Err(thrown(error.0))),
    };
    let audience_display = match app.shell.display_of(Some(audience_window)) {
        Ok(display) => display,
        Err(error) => return responder.reply(Err(thrown(error.0))),
    };
    if presenter_display.id == audience_display.id {
        return responder.reply(Ok(IpcValue::Bool(false)));
    }
    app.shell.presenter_window_owned(
        &source,
        &destination,
        &cx.cancel,
        &cx.cancel,
        PresenterWindowAction::SwapDisplays {
            presenter,
            audience: audience_window,
            presenter_display,
            audience_display,
        },
        Box::new(move |answer| {
            responder.reply(answer.map(|()| IpcValue::Bool(true)).map_err(thrown))
        }),
    );
}

pub(in crate::slides) fn show_fullscreen(
    app: &App,
    cx: &CallContext,
    args: Args,
    responder: Responder,
) {
    let source = match super::super::session::render_owner(cx).and_then(|source| {
        if app.state.borrow().has_session(source.view) {
            app.capture_save_owner(source.view, source)
        } else {
            Ok(source)
        }
    }) {
        Ok(source) => source,
        Err(error) => return responder.reply(Err(error)),
    };
    let Some(window) = app.shell.host_window_for(source.view) else {
        return responder.reply(Ok(IpcValue::Undefined));
    };
    let on = crate::value::truthy(args.get(0));
    let pending = CancelToken::default();
    let (ticket, previous) = {
        let mut state = app.state.borrow_mut();
        state.show_fs_ticket += 1;
        (
            state.show_fs_ticket,
            state.show_fs_pending.replace(pending.clone()),
        )
    };
    if let Some(previous) = previous {
        previous.cancel();
    }
    if on {
        let services = app.services.clone();
        let index = app.registry_index;
        let callback_source = source.clone();
        let cancel = cx.cancel.clone();
        let callback_pending = pending.clone();
        app.shell.presenter_window_owned(
            &source,
            &source,
            &cx.cancel,
            &pending,
            PresenterWindowAction::ShowFullscreen { window, on: true },
            Box::new(move |answer| {
                if let Err(error) = answer {
                    return responder.reply(Err(thrown(error)));
                }
                let workers = services.workers.clone();
                workers.spawn(Box::new(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    super::super::post_to(&services, index, move |app| {
                        app.finish_fullscreen(
                            ticket,
                            callback_source,
                            cancel,
                            callback_pending,
                            PresenterWindowAction::Refocus { window },
                            responder,
                        )
                    });
                }));
            }),
        );
    } else {
        let services = app.services.clone();
        let index = app.registry_index;
        let cancel = cx.cancel.clone();
        app.services.workers.spawn(Box::new(move || {
            std::thread::sleep(SHOW_RELEASE_DELAY);
            super::super::post_to(&services, index, move |app| {
                app.finish_fullscreen(
                    ticket,
                    source,
                    cancel,
                    pending,
                    PresenterWindowAction::ShowFullscreen { window, on: false },
                    responder,
                )
            });
        }));
    }
}

impl App {
    fn finish_fullscreen(
        &self,
        ticket: u64,
        source: RenderOwner,
        cancel: CancelToken,
        pending: CancelToken,
        action: PresenterWindowAction,
        responder: Responder,
    ) {
        if self.state.borrow().show_fs_ticket != ticket {
            return responder.reply(Err(thrown("The fullscreen request was superseded")));
        }
        self.shell.presenter_window_owned(
            &source,
            &source,
            &cancel,
            &pending,
            action,
            Box::new(move |answer| {
                responder.reply(answer.map(|()| IpcValue::Undefined).map_err(thrown))
            }),
        );
    }
}
