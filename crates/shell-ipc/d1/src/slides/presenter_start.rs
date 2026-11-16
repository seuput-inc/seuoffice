use super::super::engine::{DeckId, EngineError, RenderOwner};
use super::super::state::{App, PresenterShow, StagedBinding};
use crate::value::{Args, obj, thrown};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, Responder};
use shell_contract::runtime::CancelToken;
use shell_contract::{IpcError, IpcValue, ViewId, WindowId};

type Audience = Result<Option<(RenderOwner, WindowId)>, EngineError>;

pub(in crate::slides) fn presenter_start(
    app: &App,
    cx: &CallContext,
    _args: Args,
    responder: Responder,
) {
    let view = cx.sender.view;
    let source = match super::super::session::render_owner(cx) {
        Ok(owner) => owner,
        Err(error) => return responder.reply(Err(error)),
    };
    let captured = {
        let state = app.state.borrow();
        state.slot_of(view).and_then(|slot| {
            state
                .slot(slot)
                .map(|session| (slot, session.id, session.deck()))
        })
    };
    let source = if captured.is_some() {
        match app.capture_save_owner(view, source) {
            Ok(owner) => owner,
            Err(error) => return responder.reply(Err(error)),
        }
    } else {
        source
    };
    {
        let mut state = app.state.borrow_mut();
        if let Some(show) = state.presenter_shows.get_mut(&view) {
            if !show
                .owner
                .context
                .renderer()
                .zip(source.context.renderer())
                .is_some_and(|(expected, actual)| expected.same_generation(actual))
            {
                return responder.reply(Err(thrown(
                    "The presenter belongs to another native document",
                )));
            }
            if show.starting {
                show.start_waiters.push(responder);
            } else {
                responder.reply(Ok(obj([(
                    "audience",
                    IpcValue::Bool(show.audience.is_some()),
                )])));
            }
            return;
        }
    }
    let display = app.shell.display_of(app.shell.host_window_for(view));
    let display = match display {
        Ok(display) => display,
        Err(error) => return responder.reply(Err(thrown(error.0))),
    };
    let displays = match app.shell.displays() {
        Ok(displays) => displays,
        Err(error) => return responder.reply(Err(thrown(error.0))),
    };
    let external = displays
        .into_iter()
        .find(|candidate| candidate.id != display.id);
    let ticket = {
        let mut state = app.state.borrow_mut();
        state.next_ticket += 1;
        let ticket = state.next_ticket;
        state.presenter_shows.insert(
            view,
            PresenterShow {
                audience: None,
                audience_owner: None,
                last_sync: None,
                owner: source.clone(),
                ticket,
                starting: true,
                start_waiters: vec![responder],
            },
        );
        ticket
    };
    let Some((slot, session, deck)) = captured else {
        return app.answer_audience_start(view, ticket, Ok(false));
    };
    let Some(external) = external else {
        return app.answer_audience_start(view, ticket, Ok(false));
    };
    let services = app.services.clone();
    let index = app.registry_index;
    let cancel = cx.cancel.clone();
    let callback_source = source.clone();
    app.shell.open_audience_window(
        external,
        source,
        cancel.clone(),
        Box::new(move |created| {
            super::super::post_to(&services, index, move |app| {
                app.audience_started(
                    view,
                    ticket,
                    slot,
                    session,
                    deck,
                    callback_source,
                    cancel,
                    created,
                )
            });
        }),
    );
}

impl App {
    fn answer_audience_start(
        &self,
        presenter: ViewId,
        ticket: u64,
        result: Result<bool, IpcError>,
    ) {
        let waiters = {
            let mut state = self.state.borrow_mut();
            let Some(show) = state
                .presenter_shows
                .get_mut(&presenter)
                .filter(|show| show.ticket == ticket)
            else {
                return;
            };
            show.starting = false;
            let waiters = std::mem::take(&mut show.start_waiters);
            if result.is_err() {
                state.presenter_shows.remove(&presenter);
            }
            waiters
        };
        for waiter in waiters {
            waiter.reply(
                result
                    .clone()
                    .map(|audience| obj([("audience", IpcValue::Bool(audience))])),
            );
        }
    }

    fn audience_started(
        &self,
        presenter: ViewId,
        ticket: u64,
        slot: usize,
        session: u64,
        deck: DeckId,
        source: RenderOwner,
        cancel: CancelToken,
        created: Audience,
    ) {
        let (audience, window) = match created {
            Ok(Some(created)) => created,
            Ok(None) => return self.answer_audience_start(presenter, ticket, Ok(false)),
            Err(error) => {
                return self.answer_audience_start(presenter, ticket, Err(thrown(error.0)));
            }
        };
        let result = (|| {
            let effects = self
                .services
                .hosts
                .get::<OwnedEffectsHost>()
                .cloned()
                .ok_or_else(|| thrown("The native audience effect host is unavailable"))?;
            let staged = StagedBinding::new(self.engine.clone(), deck, &audience)
                .map_err(|error| thrown(error.0))?;
            effects
                .with_owned_effect(&source, &audience, &cancel, &cancel, &mut || {
                    let mut state = self.state.borrow_mut();
                    if !state
                        .presenter_shows
                        .get(&presenter)
                        .is_some_and(|show| show.ticket == ticket && show.starting)
                        || state.slot_of(presenter) != Some(slot)
                        || !state
                            .slot(slot)
                            .is_some_and(|s| s.id == session && s.deck() == deck)
                        || state.has_session(audience.view)
                    {
                        return Err(
                            "The presenter session was replaced while its audience was opening"
                                .into(),
                        );
                    }
                    state.attach_initial_view(audience.clone(), slot)?;
                    let audience_owner = state
                        .owner_of(audience.view)
                        .cloned()
                        .ok_or_else(|| "The audience binding was released".to_owned())?;
                    if let Some(show) = state.presenter_shows.get_mut(&presenter) {
                        show.audience = Some((audience.view, window));
                        show.audience_owner = Some(audience_owner);
                    }
                    state.audience_presenter.insert(audience.view, presenter);
                    state.viewers.insert(audience.view);
                    Ok(())
                })
                .map_err(thrown)?;
            staged.commit(None).map_err(|error| thrown(error.0))?;
            Ok(())
        })();
        if let Err(error) = result {
            self.shell.close_audience_window(window);
            return self.answer_audience_start(presenter, ticket, Err(error));
        }
        self.answer_audience_start(presenter, ticket, Ok(true));
    }
}
