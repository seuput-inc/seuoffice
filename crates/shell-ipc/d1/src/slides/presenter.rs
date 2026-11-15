//! `presenter-show.ts` (the audience window on the external display and the
//! presenter-to-audience forwarding) and `slides:show-fullscreen`.

use std::time::Duration;

use shell_contract::ipc::CallContext;
use shell_contract::{IpcError, IpcValue, ViewId};

use super::state::App;
use crate::value::{Args, or_null, thrown};

type Reply = Result<IpcValue, IpcError>;

impl App {
    fn detach_bound_view(&self, view: ViewId) {
        let binding = self.state.borrow().binding_of(view);
        if let Some((deck, owner)) = binding {
            if let Err(error) = self.engine.unbind_view(deck, &owner) {
                eprintln!("[slides] view unbind failed: {}", error.0);
            }
        }
        self.state.borrow_mut().detach_view(view);
    }

    /// `closePresenterShow(presenterId)`
    pub fn close_presenter_show(&self, presenter: ViewId) {
        let show = self.state.borrow_mut().presenter_shows.remove(&presenter);
        if let Some(show) = show {
            for waiter in show.start_waiters {
                waiter.reply(Err(thrown("The presenter show was closed")));
            }
            if let Some((view, window)) = show.audience {
                self.detach_bound_view(view);
                {
                    let mut state = self.state.borrow_mut();
                    state.viewers.remove(&view);
                    state.audience_presenter.remove(&view);
                }
                self.shell.close_audience_window(window);
            }
        }
    }

    /// The audience window closed (`win.on('closed')`)
    pub fn audience_closed(&self, audience: ViewId) {
        self.detach_bound_view(audience);
        let mut state = self.state.borrow_mut();
        state.viewers.remove(&audience);
        state.audience_presenter.remove(&audience);
        for show in state.presenter_shows.values_mut() {
            if show.audience.is_some_and(|(view, _)| view == audience) {
                show.audience = None;
                show.audience_owner = None;
            }
        }
    }
}

#[path = "presenter_start.rs"]
mod start;
pub(super) use start::presenter_start;
#[path = "presenter_ui.rs"]
mod ui;
pub(super) use ui::{presenter_swap, show_fullscreen};

fn show_pair(
    app: &App,
    cx: &CallContext,
    from_audience: bool,
) -> Result<Option<(super::engine::RenderOwner, super::engine::RenderOwner)>, IpcError> {
    let incoming = super::session::render_owner(cx)?;
    let pair = {
        let state = app.state.borrow();
        let presenter = if from_audience {
            state.audience_presenter.get(&incoming.view).copied()
        } else {
            Some(incoming.view)
        };
        let Some(show) = presenter.and_then(|view| state.presenter_shows.get(&view)) else {
            return Ok(None);
        };
        let Some(audience) = show.audience_owner.clone() else {
            return Ok(None);
        };
        if from_audience {
            (audience, show.owner.clone())
        } else {
            (show.owner.clone(), audience)
        }
    };
    if !incoming
        .context
        .renderer()
        .zip(pair.0.context.renderer())
        .is_some_and(|(actual, expected)| actual.same_generation(expected))
        || incoming
            .context
            .document_lease()
            .zip(pair.0.context.document_lease())
            .is_some_and(|(actual, expected)| !actual.same_generation(expected))
    {
        return Err(thrown(
            "The presenter message belongs to a replaced document",
        ));
    }
    let source = app.capture_save_owner(incoming.view, incoming)?;
    Ok(Some((source, pair.1)))
}

fn forward(
    app: &App,
    cx: &CallContext,
    source: &super::engine::RenderOwner,
    target: &super::engine::RenderOwner,
    channel: &str,
    args: Vec<IpcValue>,
) -> Result<(), IpcError> {
    let effects = app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .ok_or_else(|| thrown("The presenter effect host is unavailable"))?;
    effects
        .emit_owned(source, target, &cx.cancel, &cx.cancel, channel, args)
        .map_err(thrown)
}

pub fn presenter_sync(app: &App, cx: &CallContext, args: Args) -> Result<(), IpcError> {
    let Some((source, audience)) = show_pair(app, cx, false)? else {
        return Ok(());
    };
    let effects = app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .ok_or_else(|| thrown("The presenter effect host is unavailable"))?;
    effects
        .with_owned_effect(&source, &audience, &cx.cancel, &cx.cancel, &mut || {
            let mut state = app.state.borrow_mut();
            let show = state
                .presenter_shows
                .get_mut(&source.view)
                .ok_or_else(|| "The presenter show was closed".to_owned())?;
            show.last_sync = Some(args.get(0).clone());
            Ok(())
        })
        .map_err(thrown)?;
    forward(
        app,
        cx,
        &source,
        &audience,
        "slides:show-sync",
        vec![args.get(0).clone()],
    )
}

pub fn presenter_ink(app: &App, cx: &CallContext, args: Args) -> Result<(), IpcError> {
    if let Some((source, audience)) = show_pair(app, cx, false)? {
        forward(
            app,
            cx,
            &source,
            &audience,
            "slides:show-ink",
            vec![args.get(0).clone()],
        )?;
    }
    Ok(())
}

pub fn presenter_end(app: &App, cx: &CallContext, _args: Args) -> Reply {
    let incoming = super::session::render_owner(cx)?;
    if let Some(show) = app.state.borrow().presenter_shows.get(&incoming.view) {
        if !incoming
            .context
            .renderer()
            .zip(show.owner.context.renderer())
            .is_some_and(|(actual, expected)| actual.same_generation(expected))
        {
            return Err(thrown("The presenter document was replaced"));
        }
    }
    app.close_presenter_show(incoming.view);
    Ok(IpcValue::Undefined)
}

pub fn audience_ready(app: &App, cx: &CallContext, _args: Args) -> Reply {
    let Some((audience, presenter)) = show_pair(app, cx, true)? else {
        return Ok(IpcValue::Null);
    };
    let effects = app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .ok_or_else(|| thrown("The presenter effect host is unavailable"))?;
    let mut sync = None;
    effects
        .with_owned_effect(&audience, &presenter, &cx.cancel, &cx.cancel, &mut || {
            sync = app
                .state
                .borrow()
                .presenter_shows
                .get(&presenter.view)
                .and_then(|show| show.last_sync.clone());
            Ok(())
        })
        .map_err(thrown)?;
    Ok(or_null(sync))
}

pub fn audience_nav(app: &App, cx: &CallContext, args: Args) -> Result<(), IpcError> {
    if let Some((audience, presenter)) = show_pair(app, cx, true)? {
        forward(
            app,
            cx,
            &audience,
            &presenter,
            "slides:audience-nav",
            vec![args.get(0).clone()],
        )?;
    }
    Ok(())
}

/// How long a show's release waits, so React strict-mode remounts and
/// presenter-to-show handoffs do not bounce the window
const SHOW_RELEASE_DELAY: Duration = Duration::from_millis(150);
