use shell_contract::editors::{SaveMode, Then};
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::CancelToken;
use shell_contract::{IpcError, ViewId, WindowId};

use super::core::{Core, OwnedCloseWaiter};
use super::dialogs::message_box;
use super::recovery::node_err;
use super::recovery_policy::session_recovery_identity;
use crate::value::thrown;

impl Core {
    fn finish_owned_close(&self, id: u64, result: Result<bool, IpcError>) {
        let waiter = self.state.borrow_mut().owned_closes.remove(&id);
        if let Some(waiter) = waiter {
            waiter.scope.pending.cancel();
            drop(waiter.subscriptions);
            (waiter.then)(result);
        }
    }

    pub(super) fn request_close_owned(
        &self, view: ViewId, parent: Option<WindowId>, origin: NativeRequestOwner,
        cancel: CancelToken, then: Then<bool>,
    ) {
        let captured = {
            let state = self.state.borrow();
            state.tab(view).and_then(|tab| tab.native_owner.clone()
                .map(|owner| (owner, tab.document.clone())))
        };
        let Some((destination, document)) = captured else {
            return then(Err(thrown("The workbook has no captured native renderer")));
        };
        let Some(effects) = self.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return then(Err(thrown("Native close effects are unavailable")));
        };
        let destination = match destination.context.renderer()
            .ok_or_else(|| "The workbook renderer is unavailable".to_owned())
            .and_then(|generation| effects.capture_owner(view, generation)) {
                Ok(owner) => owner,
                Err(error) => return then(Err(thrown(error))),
            };
        let scope = match crate::owned_save::OwnedSave::new(origin, destination, cancel, document, effects) {
            Ok(scope) => scope,
            Err(error) => return then(Err(error)),
        };
        let id = {
            let mut state = self.state.borrow_mut();
            state.next_token = state.next_token.wrapping_add(1);
            state.next_token
        };
        let mut subscriptions = Vec::new();
        for loss in [scope.authority.loss_token(), scope.destination.loss_token(), Some(scope.cancel.clone())].into_iter().flatten() {
            let pending = scope.pending.clone();
            let env = self.env.clone();
            subscriptions.push(loss.on_cancel(move || {
                pending.cancel();
                env.post(move |core| core.finish_owned_close(id, Err(thrown("The workbook close owner was released"))));
            }));
        }
        self.state.borrow_mut().owned_closes.insert(id, OwnedCloseWaiter { scope: scope.clone(), then, subscriptions });
        if let Err(error) = scope.check() { return self.finish_owned_close(id, Err(error)); }
        let count = match scope.effect(|| Ok(self.pending_edit_count(view))) {
            Ok(count) => count,
            Err(error) => return self.finish_owned_close(id, Err(error)),
        };
        if main_core::sheets::close_guard::close_guard_decision(
            count, !self.env.alive(view), self.state.borrow().shutdown.active() || self.env.app_shutting_down(),
        ) == main_core::sheets::close_guard::CloseGuardDecision::Proceed {
            return self.finish_owned_close(id, Ok(true));
        }
        let tm = &self.env.tm;
        let options = message_box(
            "warning", tm.with("closeUnsavedMsg", &[("count", jscompat::JsonValue::Number(count))]),
            Some(tm.t("closeUnsavedDetail")),
            vec![tm.t("menuSave"), tm.t("btnDontSave"), tm.t("btnCancel")], 0, Some(2),
        );
        let env = self.env.clone();
        scope.message_dialog(
            &self.env.dialogs, parent, options,
            Box::new(move |result| env.post(move |core| core.after_owned_close_prompt(id, view, result))),
        );
    }

    fn after_owned_close_prompt(&self, id: u64, view: ViewId, answer: Result<shell_contract::hosts::MessageBoxAnswer, String>) {
        let scope = self.state.borrow().owned_closes.get(&id).map(|waiter| waiter.scope.clone());
        let Some(scope) = scope else { return };
        if let Err(error) = scope.check() { return self.finish_owned_close(id, Err(error)); }
        let answer = match answer { Ok(answer) => answer.response, Err(error) => return self.finish_owned_close(id, Err(thrown(error))) };
        match answer {
            2 => self.finish_owned_close(id, Ok(false)),
            1 => self.discard_owned_close(id, view, scope),
            _ => {
                if self.state.borrow().shutdown.active() || self.env.app_shutting_down() || !self.env.alive(view) {
                    return self.finish_owned_close(id, Ok(true));
                }
                let core = self.clone();
                self.request_owned_save(view, SaveMode::Save, None, scope.authority.clone(), scope.pending.clone(), None, Some(scope.clone()),
                    Box::new(move |result| core.finish_owned_close(id, result)));
            }
        }
    }

    fn discard_owned_close(&self, id: u64, view: ViewId, scope: crate::owned_save::OwnedSave) {
        let snapshot = scope.effect(|| {
            let mut state = self.state.borrow_mut();
            let sessions = state.tab(view).ok_or_else(|| thrown("The workbook tab was closed"))?
                .sessions.iter().map(|(_, session)| session.clone()).collect::<Vec<_>>();
            let recovery = state.recovery_of(view);
            recovery.discarded = true;
            Ok((sessions, recovery.written.clone(), recovery.held.clone()))
        });
        let (sessions, mut identities, held) = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => return self.finish_owned_close(id, Err(error)),
        };
        let env = self.env.clone();
        let worker = env.clone();
        env.spawn(move || {
            let result = (|| -> Result<(), IpcError> {
                for session in &sessions {
                    if let Some(identity) = session_recovery_identity(session)
                        && let Some(key) = worker.copy_key(&identity) {
                        match identities.iter_mut().find(|(existing, _)| *existing == key) {
                            Some(entry) => entry.1 = identity,
                            None => identities.push((key, identity)),
                        }
                    }
                }
                let selected = identities.into_iter().filter_map(|(key, identity)|
                    (!held.contains(&key)).then_some(identity)).collect::<Vec<_>>();
                let core_env = worker.clone();
                let admission: main_core::packages::electron_utils::atomic_write::owned::Admission = std::sync::Arc::new(move |effect| {
                    core_env.core(move |core| {
                        let scope = core.state.borrow().owned_closes.get(&id)
                            .map(|waiter| waiter.scope.clone())
                            .ok_or_else(|| thrown("The workbook close is no longer pending"))?;
                        scope.effect(|| Ok(effect()))
                    }).map_err(|error| main_core::node_fs::NodeError { code: None, message: error.message })
                });
                crate::owned_recovery::delete_copies(
                    &worker.recovery, &worker.user_data_dir(), &selected,
                    main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason::DontSave,
                    &admission,
                ).map_err(node_err)?;
                Ok(())
            })();
            worker.post(move |core| {
                let result = result.and_then(|()| scope.effect(|| {
                    let mut state = core.state.borrow_mut();
                    let recovery = state.recovery_of(view);
                    recovery.written.clear();
                    recovery.keep.clear();
                    Ok(true)
                }));
                core.finish_owned_close(id, result);
            });
        });
    }
}
