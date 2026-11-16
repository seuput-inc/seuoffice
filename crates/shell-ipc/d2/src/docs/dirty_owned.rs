//! Tokenized dirty-state queries retain both the requesting and target renderer lifetimes.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use shell_contract::editors::Then;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelSubscription, CancelToken, RendererGeneration};
use shell_contract::{IpcValue, ViewId};

use super::Docs;
use crate::kit::{self, Timer};

struct Query {
    source: NativeRequestOwner,
    target: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    cancel: CancelToken,
    timer: Timer,
    _loss: Vec<CancelSubscription>,
    then: Then<super::close::CloseState>,
    timeout_is_unresponsive: bool,
    pending: CancelToken,
}

#[derive(Default)]
pub(super) struct Queries(RefCell<HashMap<String, Query>>);

impl Queries {
    fn fail(&self, token: &str, reason: &str) {
        let query = self.0.borrow_mut().remove(token);
        if let Some(query) = query {
            kit::clear(query.timer);
            query.pending.cancel();
            (query.then)(Err(kit::error(reason)));
        }
    }

    fn timeout(&self, token: &str) {
        let query = self.0.borrow_mut().remove(token);
        if let Some(query) = query {
            kit::clear(query.timer);
            let cancelled = query.pending.is_cancelled();
            query.pending.cancel();
            if cancelled {
                (query.then)(Err(kit::error("The dirty-state query was cancelled")));
            } else if query.timeout_is_unresponsive {
                (query.then)(Ok(super::close::CloseState {
                    dirty: true,
                    unresponsive: true,
                    ..Default::default()
                }));
            } else {
                (query.then)(Err(kit::error("The document renderer did not answer")));
            }
        }
    }

    pub(super) fn fail_view(&self, view: ViewId) {
        let tokens = self
            .0
            .borrow()
            .iter()
            .filter(|(_, q)| {
                q.source.view == view || q.participants.iter().any(|owner| owner.view == view)
            })
            .map(|(token, _)| token.clone())
            .collect::<Vec<_>>();
        for token in tokens {
            self.fail(&token, "The dirty-state query view was destroyed");
        }
    }

    pub(super) fn fail_generation(&self, view: ViewId, old: &RendererGeneration) {
        let tokens = self
            .0
            .borrow()
            .iter()
            .filter(|(_, q)| {
                std::iter::once(&q.source)
                    .chain(q.participants.iter())
                    .any(|owner| {
                        owner.view == view
                            && owner
                                .context
                                .renderer()
                                .is_some_and(|generation| generation.same_generation(old))
                    })
            })
            .map(|(token, _)| token.clone())
            .collect::<Vec<_>>();
        for token in tokens {
            self.fail(&token, "The dirty-state query renderer was replaced");
        }
    }
}

pub(super) fn start(
    docs: &Rc<Docs>,
    view: ViewId,
    source: NativeRequestOwner,
    cancel: CancelToken,
    then: Then<bool>,
) {
    let Some(source_generation) = source
        .context
        .renderer()
        .filter(|generation| generation.is_active())
        .cloned()
    else {
        return then(Err(kit::error(
            "The dirty-state query requester is unavailable",
        )));
    };
    let Some(target_generation) = docs
        .services
        .views
        .renderer_generation(view)
        .filter(|generation| generation.is_active())
    else {
        return then(Err(kit::error("The document renderer is unavailable")));
    };
    if !docs.alive(view)
        || !docs
            .services
            .views
            .sender_of(view)
            .is_some_and(|sender| sender.surface == shell_contract::Surface::Docs)
        || !docs
            .services
            .views
            .renderer_generation(source.view)
            .is_some_and(|current| current.same_generation(&source_generation))
    {
        return then(Err(kit::error("The dirty-state query view is unavailable")));
    }
    let target = match docs
        .shell
        .capture_dirty_query_target(view, &target_generation)
    {
        Ok(target) => target,
        Err(error) => return then(Err(kit::error(error))),
    };
    start_captured(
        docs,
        source,
        Vec::new(),
        target,
        cancel,
        CancelToken::default(),
        false,
        Box::new(move |result| then(result.map(|state| state.dirty))),
    );
}

pub(super) fn start_captured(
    docs: &Rc<Docs>,
    source: NativeRequestOwner,
    mut participants: Vec<NativeRequestOwner>,
    target: NativeRequestOwner,
    cancel: CancelToken,
    parent_pending: CancelToken,
    timeout_is_unresponsive: bool,
    then: Then<super::close::CloseState>,
) {
    if !source
        .context
        .renderer()
        .is_some_and(|generation| generation.is_active())
    {
        return then(Err(kit::error(
            "The dirty-state query requester is unavailable",
        )));
    }
    if !docs.alive(target.view) {
        return then(Err(kit::error("The dirty-state query view is unavailable")));
    }
    let target = match docs.checked_document_owner(target.view, target) {
        Ok(target) => target,
        Err(error) => return then(Err(error)),
    };
    let Some(effects) = docs
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
    else {
        return then(Err(kit::error("Native owned effects are unavailable")));
    };
    let target_index = participants.len();
    participants.push(target.clone());
    let mut losses = vec![cancel.clone(), parent_pending];
    for owner in std::iter::once(&source).chain(participants.iter()) {
        let Some(loss) = owner.loss_token() else {
            return then(Err(kit::error(
                "The dirty-state query owner lease is unavailable",
            )));
        };
        let Some(generation) = owner.context.renderer() else {
            return then(Err(kit::error(
                "The dirty-state query renderer is unavailable",
            )));
        };
        losses.push(loss);
        losses.push(generation.loss_token());
    }
    let token = format!("dirty:{}", kit::unique());
    let weak = Rc::downgrade(docs);
    let timeout_token = token.clone();
    let timer = kit::after(
        &docs.services,
        std::time::Duration::from_millis(2000),
        move || {
            if let Some(docs) = weak.upgrade() {
                docs.close.owned_queries.timeout(&timeout_token);
            }
        },
    );
    let mut subscriptions = Vec::new();
    let pending = CancelToken::default();
    for loss in losses {
        let weak = Rc::downgrade(docs);
        let key = token.clone();
        let pending = pending.clone();
        let callback = kit::back_on_core(&docs.services, move |_: ()| {
            if let Some(docs) = weak.upgrade() {
                docs.close
                    .owned_queries
                    .fail(&key, "The dirty-state query was cancelled");
            }
        });
        subscriptions.push(loss.on_cancel(move || {
            pending.cancel();
            callback(());
        }));
    }
    docs.close.owned_queries.0.borrow_mut().insert(
        token.clone(),
        Query {
            source: source.clone(),
            target: target.clone(),
            participants: participants.clone(),
            effects: effects.clone(),
            cancel: cancel.clone(),
            timer,
            _loss: subscriptions,
            then,
            timeout_is_unresponsive,
            pending: pending.clone(),
        },
    );
    if pending.is_cancelled() {
        docs.close
            .owned_queries
            .fail(&token, "The dirty-state query was cancelled");
        return;
    }
    if let Err(error) = effects.emit_owned_many(
        &source,
        &participants,
        target_index,
        &cancel,
        &pending,
        "docs:close-check",
        vec![IpcValue::String(token.clone())],
    ) {
        docs.close.owned_queries.fail(&token, &error);
    }
}

/// A tokenized reply cannot settle an unowned close check or another query.
pub(super) fn answer(
    docs: &Docs,
    view: ViewId,
    generation: Option<&RendererGeneration>,
    token: &str,
    state: &IpcValue,
) {
    let query = {
        let mut pending = docs.close.owned_queries.0.borrow_mut();
        let matches = pending.get(token).is_some_and(|q| {
            q.target.view == view
                && generation.is_some_and(|generation| {
                    generation.is_active()
                        && q.target
                            .context
                            .renderer()
                            .is_some_and(|expected| expected.same_generation(generation))
                })
        });
        if matches { pending.remove(token) } else { None }
    };
    let Some(query) = query else { return };
    kit::clear(query.timer);
    let dirty = match state {
        IpcValue::Bool(dirty) => *dirty,
        other => kit::field(other, "dirty") == &IpcValue::Bool(true),
    };
    let mut accepted = None;
    let result = query
        .effects
        .with_owned_effect_many(
            &query.source,
            &query.participants,
            &query.cancel,
            &query.pending,
            &mut || {
                accepted = Some(super::close::CloseState {
                    dirty,
                    auto_save: kit::field(state, "autoSave") == &IpcValue::Bool(true),
                    file_path: kit::as_str(kit::field(state, "filePath")).map(str::to_owned),
                    unresponsive: false,
                });
                Ok(())
            },
        )
        .and_then(|()| accepted.ok_or_else(|| "The dirty-state query was not admitted".into()))
        .map_err(kit::error);
    query.pending.cancel();
    (query.then)(result);
}
