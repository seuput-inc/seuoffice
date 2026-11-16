use super::super::{OwnedPasswordWindowActions, SavedPasswordWindowFactory, with};
use super::{Password, Settle};
use crate::kit;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::{
    IpcValue,
    ipc::{CallContext, NativeRequestOwner},
    runtime::{CancelSubscription, CancelToken},
};
use std::sync::Arc;

const LOST: &str = "Password request owner is no longer available";

#[derive(Clone)]
enum Authority {
    Renderer {
        source: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        effects: OwnedEffectsHost,
    },
    Prepared {
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        cancel: CancelToken,
        pending: CancelToken,
        effects: OwnedEffectsHost,
    },
    Saved {
        document: shell_contract::ipc::NativeSavedDocument,
        factory: SavedPasswordWindowFactory,
        effects: OwnedEffectsHost,
    },
}
impl Authority {
    fn commit(
        &self,
        prompt: Option<&NativeRequestOwner>,
        action: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Renderer {
                source,
                destination,
                cancel,
                pending,
                effects,
            } => match prompt {
                Some(prompt) => effects.with_owned_transition_many(
                    source,
                    &[destination.clone(), prompt.clone()],
                    cancel,
                    pending,
                    &mut |_| action(),
                ),
                None => effects.with_owned_effect(source, destination, cancel, pending, action),
            },
            Self::Prepared {
                target,
                snapshot,
                cancel,
                pending,
                effects,
            } => match prompt {
                Some(prompt) => effects.with_recovery_dialog_snapshot(
                    target,
                    snapshot,
                    prompt,
                    prompt.context.renderer().ok_or(LOST)?,
                    cancel,
                    pending,
                    action,
                ),
                None => effects.with_recovery_seed_snapshot(target, snapshot, action),
            },
            Self::Saved {
                document, effects, ..
            } => {
                let target = document.target();
                let participants = prompt.cloned().into_iter().collect::<Vec<_>>();
                effects.with_native_saved_document_input(
                    document,
                    &participants,
                    target.cancel(),
                    target.pending(),
                    &[],
                    &mut |_| action(),
                )
            }
        }
    }
    fn emit(&self, prompt: &NativeRequestOwner, state: IpcValue) -> Result<(), String> {
        match self {
            Self::Renderer {
                source,
                destination,
                cancel,
                pending,
                effects,
            } => effects.emit_owned_many(
                source,
                &[destination.clone(), prompt.clone()],
                1,
                cancel,
                pending,
                "pdf-password:changed",
                vec![state],
            ),
            Self::Prepared {
                target,
                snapshot,
                cancel,
                pending,
                effects,
            } => effects.emit_recovery_dialog_snapshot(
                target,
                snapshot,
                prompt,
                prompt.context.renderer().ok_or(LOST)?,
                cancel,
                pending,
                "pdf-password:changed",
                vec![state],
            ),
            Self::Saved {
                document, effects, ..
            } => {
                let target = document.target();
                effects.emit_native_saved_document_input(
                    document,
                    std::slice::from_ref(prompt),
                    0,
                    target.cancel(),
                    target.pending(),
                    &[],
                    "pdf-password:changed",
                    vec![state],
                )
            }
        }
    }
    fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Renderer {
                    source,
                    destination,
                    cancel,
                    pending,
                    ..
                },
                Self::Renderer {
                    source: next_source,
                    destination: next_destination,
                    cancel: next_cancel,
                    pending: next_pending,
                    ..
                },
            ) => {
                same_owner(source, next_source)
                    && same_owner(destination, next_destination)
                    && cancel.same_token(next_cancel)
                    && pending.same_token(next_pending)
            }
            (
                Self::Prepared {
                    target, snapshot, ..
                },
                Self::Prepared {
                    target: next,
                    snapshot: next_snapshot,
                    ..
                },
            ) => target.same_binding(next) && snapshot.same_snapshot(next_snapshot),
            (
                Self::Saved {
                    document, factory, ..
                },
                Self::Saved {
                    document: next,
                    factory: next_factory,
                    ..
                },
            ) => {
                document.target().same_target(next.target())
                    && same_owner(document.owner(), next.owner())
                    && Arc::ptr_eq(factory, next_factory)
            }
            _ => false,
        }
    }
    fn losses(&self) -> Result<Vec<CancelToken>, String> {
        match self {
            Self::Renderer {
                source,
                destination,
                cancel,
                pending,
                ..
            } => Ok(vec![
                source.loss_token().ok_or(LOST)?,
                destination.loss_token().ok_or(LOST)?,
                cancel.clone(),
                pending.clone(),
            ]),
            Self::Prepared {
                snapshot,
                cancel,
                pending,
                ..
            } => {
                let mut tokens = snapshot.preparation_loss_tokens()?;
                tokens.extend([cancel.clone(), pending.clone()]);
                Ok(tokens)
            }
            Self::Saved { document, .. } => {
                let target = document.target();
                let operation = target.output().selection().binding().operation();
                let mut tokens = vec![
                    target.cancel().clone(),
                    target.pending().clone(),
                    operation.cancel().clone(),
                    operation.shutdown().clone(),
                    target.created().loss_token(),
                ];
                tokens.extend_from_slice(target.additional());
                for owner in std::iter::once(target.output().selection().binding().owner())
                    .chain(target.participants().iter())
                    .chain(std::iter::once(document.owner()))
                {
                    tokens.push(owner.loss_token().ok_or(LOST)?);
                }
                Ok(tokens)
            }
        }
    }
}

fn same_owner(first: &NativeRequestOwner, second: &NativeRequestOwner) -> bool {
    first.view == second.view
        && first
            .loss_token()
            .zip(second.loss_token())
            .is_some_and(|(first, second)| first.same_token(&second))
}

fn matching_ack(prompt: &NativeRequestOwner, cx: &CallContext) -> bool {
    let Some(native) = &cx.native else {
        return false;
    };
    if prompt.view != cx.sender.view {
        return false;
    }
    let renderer = prompt
        .context
        .renderer()
        .zip(native.renderer())
        .is_some_and(|(expected, actual)| expected.same_generation(actual));
    let document = match (prompt.context.document_lease(), native.document_lease()) {
        (Some(expected), Some(actual)) => expected.same_generation(actual),
        (None, None) => true,
        _ => false,
    };
    renderer && document
}

pub(super) struct Owned {
    key: String,
    identity: Arc<()>,
    authority: Authority,
    prompt: Option<NativeRequestOwner>,
    actions: Option<OwnedPasswordWindowActions>,
    subscriptions: Vec<CancelSubscription>,
}

impl Password {
    pub(super) fn replace_owned(&self) {
        let identity = self
            .state
            .borrow()
            .owned
            .as_ref()
            .map(|owned| owned.identity.clone());
        if let Some(identity) = identity {
            self.finish_owned(&identity, Ok(None));
        }
    }

    pub(crate) fn prompt_owned(
        &self,
        parent: Option<shell_contract::WindowId>,
        state: IpcValue,
        key: String,
        source: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        then: Settle,
    ) {
        let Some(effects) = self.ctx.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return then(Err("PDF owned effects unavailable".into()));
        };
        self.prompt_scoped(
            parent,
            state,
            key,
            Authority::Renderer {
                source,
                destination,
                cancel,
                pending,
                effects,
            },
            then,
        );
    }

    pub(crate) fn prompt_prepared(
        &self,
        parent: Option<shell_contract::WindowId>,
        state: IpcValue,
        key: String,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        then: Settle,
    ) {
        let Some(effects) = self.ctx.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return then(Err("PDF owned effects unavailable".into()));
        };
        self.prompt_scoped(
            parent,
            state,
            key,
            Authority::Prepared {
                target,
                snapshot,
                cancel: CancelToken::default(),
                pending: CancelToken::default(),
                effects,
            },
            then,
        );
    }

    pub(crate) fn prompt_saved(
        &self,
        state: IpcValue,
        key: String,
        document: shell_contract::ipc::NativeSavedDocument,
        factory: SavedPasswordWindowFactory,
        then: Settle,
    ) {
        let Some(effects) = self.ctx.services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return then(Err("PDF owned effects unavailable".into()));
        };
        self.prompt_scoped(
            None,
            state,
            key,
            Authority::Saved {
                document,
                factory,
                effects,
            },
            then,
        );
    }

    fn prompt_scoped(
        &self,
        parent: Option<shell_contract::WindowId>,
        state: IpcValue,
        key: String,
        requested: Authority,
        then: Settle,
    ) {
        let old = self.state.borrow().owned.as_ref().map(|owned| {
            (
                owned.key.clone(),
                owned.identity.clone(),
                owned.authority.clone(),
            )
        });
        if let Some((old_key, identity, authority)) = old {
            if old_key == key {
                if !authority.same_request(&requested) {
                    return then(Err(
                        "Password prompt identity does not match its original owner".into(),
                    ));
                }
            } else {
                self.finish_owned(&identity, Ok(None));
            }
        }
        if self.state.borrow().owned.is_none() && self.state.borrow().view.is_some() {
            self.close();
        }
        super::settle(&self.state, Ok(None));
        let existing = self
            .state
            .borrow()
            .owned
            .as_ref()
            .map(|owned| (owned.identity.clone(), owned.authority.clone()));
        let (identity, authority, opening) = if let Some((identity, authority)) = existing {
            (identity, authority, false)
        } else {
            let authority = requested;
            if let Err(error) = authority.commit(None, &mut || Ok(())) {
                return then(Err(error));
            }
            let identity = Arc::new(());
            self.state.borrow_mut().owned = Some(Owned {
                key,
                identity: identity.clone(),
                authority: authority.clone(),
                prompt: None,
                actions: None,
                subscriptions: Vec::new(),
            });
            (identity, authority, true)
        };
        let title = kit::as_str(kit::field(kit::field(&state, "strings"), "title"))
            .unwrap_or("")
            .to_owned();
        let current = {
            let mut state_slot = self.state.borrow_mut();
            let Some(generation) = state_slot.generation.checked_add(1) else {
                drop(state_slot);
                self.finish_owned(&identity, Err("Password generation exhausted".into()));
                return then(Err("Password generation exhausted".into()));
            };
            state_slot.generation = generation;
            let current = with(
                with(state, "busy", IpcValue::Bool(false)),
                "generation",
                IpcValue::String(generation.to_string()),
            );
            state_slot.current = Some(current.clone());
            state_slot.pending = Some(then);
            current
        };
        if !opening {
            let prompt = self
                .state
                .borrow()
                .owned
                .as_ref()
                .and_then(|owned| owned.prompt.clone());
            if let Some(prompt) = prompt {
                if let Err(error) = authority.emit(&prompt, current) {
                    return self.finish_owned(&identity, Err(error));
                }
                let focus = self
                    .state
                    .borrow()
                    .owned
                    .as_ref()
                    .and_then(|owned| owned.actions.as_ref())
                    .map(|actions| actions.focus.clone());
                if let Some(focus) = focus {
                    let this = self.clone();
                    let completion = identity.clone();
                    focus(kit::back_on_core(
                        &self.ctx.services,
                        move |result: Result<(), String>| {
                            if let Err(error) = result {
                                this.finish_owned(&completion, Err(error));
                            }
                        },
                    ));
                }
            }
            return;
        }
        let tokens = match authority.losses() {
            Ok(tokens) => tokens,
            Err(error) => return self.finish_owned(&identity, Err(error)),
        };
        for token in tokens {
            self.watch_owned(&identity, &token);
        }
        if let Err(error) = authority.commit(None, &mut || Ok(())) {
            return self.finish_owned(&identity, Err(error));
        }
        let this = self.clone();
        let completed = identity.clone();
        let done = kit::back_on_core(&self.ctx.services, move |result| {
            this.created_owned(&completed, result)
        });
        let actions = match authority {
            Authority::Renderer {
                source,
                destination,
                cancel,
                pending,
                ..
            } => Ok(self.ctx.shell.open_password_window_owned(
                parent,
                title,
                source,
                destination,
                cancel,
                pending,
                done,
            )),
            Authority::Prepared {
                target,
                snapshot,
                cancel,
                pending,
                ..
            } => Ok(self.ctx.shell.open_password_window_prepared(
                parent, title, target, snapshot, cancel, pending, done,
            )),
            Authority::Saved {
                document, factory, ..
            } => factory(title, document, done),
        };
        let actions = match actions {
            Ok(actions) => actions,
            Err(error) => return self.finish_owned(&identity, Err(error)),
        };
        let mut actions = Some(actions);
        {
            let mut state = self.state.borrow_mut();
            if let Some(owned) = state
                .owned
                .as_mut()
                .filter(|owned| Arc::ptr_eq(&owned.identity, &identity))
            {
                owned.actions = actions.take();
            }
        }
        if let Some(actions) = actions {
            (actions.close)();
        }
    }

    fn watch_owned(&self, identity: &Arc<()>, token: &CancelToken) {
        let this = self.clone();
        let completion = identity.clone();
        let back = kit::back_on_core(&self.ctx.services, move |(): ()| {
            this.finish_owned(&completion, Err(LOST.into()))
        });
        let subscription = token.on_cancel(move || back(()));
        let mut state = self.state.borrow_mut();
        if let Some(owned) = state
            .owned
            .as_mut()
            .filter(|owned| Arc::ptr_eq(&owned.identity, identity))
        {
            owned.subscriptions.push(subscription);
        }
    }

    fn created_owned(
        &self,
        identity: &Arc<()>,
        result: Result<(shell_contract::ViewId, NativeRequestOwner), String>,
    ) {
        if !self
            .state
            .borrow()
            .owned
            .as_ref()
            .is_some_and(|owned| Arc::ptr_eq(&owned.identity, identity))
        {
            return;
        }
        let (view, prompt) = match result {
            Ok((view, prompt)) if view == prompt.view && prompt.context.renderer().is_some() => {
                (view, prompt)
            }
            Ok(_) => {
                return self.finish_owned(
                    identity,
                    Err("Password window owner does not match the created view".into()),
                );
            }
            Err(error) => return self.finish_owned(identity, Err(error)),
        };
        let Some(loss) = prompt.loss_token() else {
            return self.finish_owned(identity, Err(LOST.into()));
        };
        let Some(authority) = self
            .state
            .borrow()
            .owned
            .as_ref()
            .map(|owned| owned.authority.clone())
        else {
            return;
        };
        let result = authority.commit(Some(&prompt), &mut || {
            let mut state = self.state.borrow_mut();
            let Some(owned) = state
                .owned
                .as_mut()
                .filter(|owned| Arc::ptr_eq(&owned.identity, identity))
            else {
                return Err(LOST.into());
            };
            owned.prompt = Some(prompt.clone());
            state.view = Some(view);
            Ok(())
        });
        if let Err(error) = result {
            return self.finish_owned(identity, Err(error));
        }
        self.watch_owned(identity, &loss);
        let current = self.state.borrow().current.clone();
        if let Some(current) = current
            && let Err(error) = authority.emit(&prompt, current)
        {
            self.finish_owned(identity, Err(error));
        }
    }

    pub(crate) fn close_owned(&self, key: &str) {
        let identity = self
            .state
            .borrow()
            .owned
            .as_ref()
            .filter(|owned| owned.key == key)
            .map(|owned| owned.identity.clone());
        if let Some(identity) = identity {
            self.finish_owned(&identity, Ok(None));
        }
    }

    fn finish_owned(&self, identity: &Arc<()>, result: Result<Option<String>, String>) {
        let (owned, pending) = {
            let mut state = self.state.borrow_mut();
            if !state
                .owned
                .as_ref()
                .is_some_and(|owned| Arc::ptr_eq(&owned.identity, identity))
            {
                return;
            }
            state.view = None;
            state.current = None;
            (state.owned.take(), state.pending.take())
        };
        let actions = owned.and_then(|mut owned| owned.actions.take());
        if let Some(pending) = pending {
            pending(result);
        }
        if let Some(actions) = actions {
            (actions.close)();
        }
    }

    fn ack_owned(
        &self,
        cx: &CallContext,
        generation: Option<&str>,
    ) -> Option<(Arc<()>, Authority, NativeRequestOwner)> {
        let state = self.state.borrow();
        let owned = state.owned.as_ref()?;
        let prompt = owned.prompt.as_ref()?;
        if !matching_ack(prompt, cx)
            || generation != Some(state.generation.to_string().as_str())
            || state.pending.is_none()
        {
            return None;
        }
        Some((
            owned.identity.clone(),
            owned.authority.clone(),
            prompt.clone(),
        ))
    }

    pub(super) fn owned_state(&self, cx: &CallContext) -> IpcValue {
        let captured = self.state.borrow().owned.as_ref().and_then(|owned| {
            owned
                .prompt
                .as_ref()
                .filter(|prompt| matching_ack(prompt, cx))
                .map(|prompt| {
                    (
                        owned.identity.clone(),
                        owned.authority.clone(),
                        prompt.clone(),
                    )
                })
        });
        let Some((identity, authority, prompt)) = captured else {
            return IpcValue::Null;
        };
        let mut result = IpcValue::Null;
        if authority
            .commit(Some(&prompt), &mut || {
                let state = self.state.borrow();
                if state
                    .owned
                    .as_ref()
                    .is_some_and(|owned| Arc::ptr_eq(&owned.identity, &identity))
                {
                    result = state.current.clone().unwrap_or(IpcValue::Null);
                }
                Ok(())
            })
            .is_err()
        {
            return IpcValue::Null;
        }
        result
    }

    pub(super) fn submit_owned(&self, cx: &CallContext, password: &str, generation: Option<&str>) {
        let Some((identity, authority, prompt)) = self.ack_owned(cx, generation) else {
            return;
        };
        let mut changed = None;
        if let Err(error) = authority.commit(Some(&prompt), &mut || {
            let mut state = self.state.borrow_mut();
            if !state
                .owned
                .as_ref()
                .is_some_and(|owned| Arc::ptr_eq(&owned.identity, &identity))
            {
                return Err(LOST.into());
            }
            if let Some(current) = state.current.take() {
                let current = with(current, "busy", IpcValue::Bool(true));
                changed = Some(current.clone());
                state.current = Some(current);
            }
            Ok(())
        }) {
            return self.finish_owned(&identity, Err(error));
        }
        if let Some(changed) = changed
            && let Err(error) = authority.emit(&prompt, changed)
        {
            return self.finish_owned(&identity, Err(error));
        }
        let mut pending = None;
        if let Err(error) = authority.commit(Some(&prompt), &mut || {
            let mut state = self.state.borrow_mut();
            if !state
                .owned
                .as_ref()
                .is_some_and(|owned| Arc::ptr_eq(&owned.identity, &identity))
            {
                return Err(LOST.into());
            }
            pending = state.pending.take();
            Ok(())
        }) {
            return self.finish_owned(&identity, Err(error));
        }
        if let Some(pending) = pending {
            pending(Ok(Some(password.to_owned())));
        }
    }

    pub(super) fn cancel_owned(&self, cx: &CallContext, generation: Option<&str>) {
        let Some((identity, authority, prompt)) = self.ack_owned(cx, generation) else {
            return;
        };
        let result = authority.commit(Some(&prompt), &mut || Ok(()));
        self.finish_owned(&identity, result.map(|()| None));
    }
}

#[cfg(test)]
mod tests;
