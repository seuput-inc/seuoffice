use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use main_core::shell::document_password_prompt::{
    AskId, DocumentPasswordPrompts, Effect, PromptId,
};
use shell_contract::ids::WindowId;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelSubscription, CancelToken};
use shell_ipc_d2::pdf::tools::PdfTools;
use shell_ipc_w::editor_files::UiLanguage;
use tauri::Manager;

use super::super::{HostContext, w::on_core, w_files::ShellLanguage};

pub(crate) type Callback = Box<dyn FnOnce(Result<Option<String>, String>) + Send>;

#[derive(Clone)]
struct PromptAuthority {
    source: NativeRequestOwner,
    destination: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
}

#[derive(Clone)]
enum PromptScope {
    Renderer(PromptAuthority),
    Prepared {
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    },
    Saved {
        document: shell_contract::ipc::NativeSavedDocument,
        factory: shell_ipc_d2::pdf::tools::SavedPasswordWindowFactory,
    },
}

struct OwnedScope {
    key: String,
    authority: Option<PromptScope>,
    losses: Vec<CancelSubscription>,
    closing: Option<crate::native::WindowClosingSubscription>,
}

#[derive(Default)]
struct State {
    model: DocumentPasswordPrompts,
    parents: HashMap<PromptId, Option<WindowId>>,
    aborted: HashSet<PromptId>,
    aborted_saved: HashSet<PromptId>,
    callbacks: HashMap<AskId, (PromptId, Callback)>,
    owned: HashMap<PromptId, OwnedScope>,
}

impl State {
    fn take_answer(&mut self, ask: AskId) -> Option<Callback> {
        self.callbacks.remove(&ask).map(|(_, callback)| callback)
    }

    fn cancel_answers(&mut self, prompt: PromptId) -> Vec<Callback> {
        self.callbacks
            .extract_if(|_, (owner, _)| *owner == prompt)
            .map(|(_, (_, callback))| callback)
            .collect()
    }
}

#[derive(Clone)]
pub(crate) struct DocumentPasswordService {
    cx: HostContext,
    state: Arc<Mutex<State>>,
    worker: std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>,
}

#[derive(Clone)]
pub(crate) struct PasswordPrompt {
    service: DocumentPasswordService,
    id: PromptId,
}

impl DocumentPasswordService {
    pub(crate) fn new(cx: &HostContext) -> Self {
        let (worker, receiver) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::spawn(move || {
            while let Ok(job) = receiver.recv() {
                job();
            }
        });
        Self {
            cx: cx.clone(),
            state: Arc::new(Mutex::new(State::default())),
            worker,
        }
    }

    pub(crate) fn create(&self, parent: Option<WindowId>, file_name: String) -> PasswordPrompt {
        let id = {
            let mut state = self.state.lock().expect("document password state");
            let id = state.model.create(&file_name);
            state.parents.insert(id, parent);
            id
        };
        let prompt = PasswordPrompt {
            service: self.clone(),
            id,
        };
        if let Some(parent) = parent {
            let owner = self
                .cx
                .views
                .label_of_window(parent)
                .and_then(|label| self.cx.app.get_window(&label));
            if let Some(owner) = owner {
                let closed = prompt.clone();
                owner.on_window_event(move |event| {
                    if matches!(event, tauri::WindowEvent::Destroyed) {
                        let closed = closed.clone();
                        std::thread::spawn(move || closed.abort());
                    }
                });
                if self
                    .cx
                    .views
                    .label_of_window(parent)
                    .and_then(|label| self.cx.app.get_window(&label))
                    .is_none()
                {
                    prompt.abort();
                }
            } else {
                prompt.abort();
            }
        }
        prompt
    }

    pub(crate) fn create_owned(
        &self,
        parent: Option<WindowId>,
        file_name: String,
        source: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
    ) -> Result<PasswordPrompt, String> {
        let source_loss = source
            .loss_token()
            .ok_or("The password source owner lease is unavailable")?;
        let destination_loss = destination
            .loss_token()
            .ok_or("The password destination owner lease is unavailable")?;
        let prompt = self.create(parent, file_name);
        let authority = PromptAuthority {
            source,
            destination,
            cancel,
            pending,
        };
        let key = jscompat::random_uuid();
        let accepted = crate::ipc::native_owner::with_paired_effect(
            &authority.source,
            &authority.destination,
            &authority.cancel,
            &authority.pending,
            || {
                self.state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .owned
                    .insert(
                        prompt.id,
                        OwnedScope {
                            key,
                            authority: Some(PromptScope::Renderer(authority.clone())),
                            losses: Vec::new(),
                            closing: None,
                        },
                    );
            },
        );
        if accepted.is_none() {
            prompt.abort();
            return Err("The password request expired before preparation".into());
        }
        let mut losses = Vec::new();
        for loss in [
            source_loss,
            destination_loss,
            authority.cancel.clone(),
            authority.pending.clone(),
        ] {
            let weak = Arc::downgrade(&self.state);
            let cx = self.cx.clone();
            let worker = self.worker.clone();
            let id = prompt.id;
            losses.push(loss.on_cancel(move || {
                let weak = weak.clone();
                let cx = cx.clone();
                let worker = worker.clone();
                crate::host::w::post_core(move |_, _| {
                    if let Some(state) = weak.upgrade() {
                        PasswordPrompt {
                            service: DocumentPasswordService { cx, state, worker },
                            id,
                        }
                        .abort();
                    }
                });
            }));
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(scope) = state
            .owned
            .get_mut(&prompt.id)
            .filter(|scope| scope.authority.is_some())
        {
            scope.losses = losses;
        }
        drop(state);
        Ok(prompt)
    }

    pub(crate) fn create_prepared(
        &self,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        file_name: String,
    ) -> Result<PasswordPrompt, String> {
        use shell_contract::runtime::Views;
        let created = self
            .cx
            .views
            .renderer_generation(target.view())
            .ok_or("The recovery window is unavailable")?;
        if !created.same_generation(target.created()) {
            return Err("The recovery window generation changed".into());
        }
        let parent = self
            .cx
            .views
            .sender_of(target.view())
            .ok_or("The recovery window is unavailable")?
            .window;
        let tokens = snapshot.preparation_loss_tokens()?;
        let prompt = self.create(Some(parent), file_name);
        let key = jscompat::random_uuid();
        let accepted =
            crate::ipc::native_owner::with_recovery_seed_snapshot(&target, &snapshot, || {
                self.state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .owned
                    .insert(
                        prompt.id,
                        OwnedScope {
                            key,
                            authority: Some(PromptScope::Prepared {
                                target: target.clone(),
                                snapshot: snapshot.clone(),
                            }),
                            losses: Vec::new(),
                            closing: None,
                        },
                    );
            });
        if accepted.is_none() {
            prompt.abort();
            return Err("The recovery request expired before password preparation".into());
        }
        let mut losses = Vec::new();
        for loss in tokens {
            let weak = Arc::downgrade(&self.state);
            let cx = self.cx.clone();
            let worker = self.worker.clone();
            let id = prompt.id;
            losses.push(loss.on_cancel(move || {
                let weak = weak.clone();
                let cx = cx.clone();
                let worker = worker.clone();
                crate::host::w::post_core(move |_, _| {
                    if let Some(state) = weak.upgrade() {
                        PasswordPrompt {
                            service: DocumentPasswordService { cx, state, worker },
                            id,
                        }
                        .abort();
                    }
                });
            }));
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(scope) = state
            .owned
            .get_mut(&prompt.id)
            .filter(|scope| scope.authority.is_some())
        {
            scope.losses = losses;
        }
        drop(state);
        Ok(prompt)
    }

    pub(crate) fn create_saved(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        file_name: String,
    ) -> Result<PasswordPrompt, String> {
        let target = document.target();
        let (parent, lifecycle) = self
            .cx
            .views
            .view_window_captured(target.view(), target.created())?;
        let output = target.output();
        let mut tokens = vec![target.cancel().clone(), target.pending().clone()];
        tokens.extend(output.loss_tokens()?);
        tokens.extend_from_slice(target.additional());
        for owner in std::iter::once(output.source_owner())
            .chain(target.participants().iter())
            .chain(std::iter::once(document.owner()))
        {
            tokens.push(
                owner
                    .loss_token()
                    .ok_or("The saved password owner lease is unavailable")?,
            );
        }
        let factory_cx = self.cx.clone();
        let factory_lifecycle = lifecycle.clone();
        let expected = target.clone();
        let factory: shell_ipc_d2::pdf::tools::SavedPasswordWindowFactory =
            Arc::new(move |title, document, done| {
                if !document.target().same_target(&expected) {
                    return Err("The password window belongs to another saved document".into());
                }
                super::open_password_window_saved(
                    &factory_cx,
                    parent,
                    factory_lifecycle.clone(),
                    title,
                    document,
                    done,
                )
            });
        let key = jscompat::random_uuid();
        let id = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "The password service is unavailable")?;
            let id = state.model.create(&file_name);
            state.parents.insert(id, Some(parent));
            state.owned.insert(
                id,
                OwnedScope {
                    key,
                    authority: None,
                    losses: Vec::new(),
                    closing: None,
                },
            );
            id
        };
        let prompt = PasswordPrompt {
            service: self.clone(),
            id,
        };
        let mut authority = Some(PromptScope::Saved {
            document: document.clone(),
            factory,
        });
        let result = crate::ipc::native_owner::with_saved_document_effect(
            &document,
            &[],
            target.cancel(),
            target.pending(),
            &[],
            &mut || {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "The password service is unavailable")?;
                let scope = state
                    .owned
                    .get_mut(&id)
                    .ok_or("The prepared password prompt is unavailable")?;
                scope.authority = authority.take();
                Ok(())
            },
        );
        if let Err(error) = result {
            prompt.abort();
            return Err(error);
        }
        let closed = prompt.clone();
        let closing = match lifecycle.when_closing(move || closed.abort()) {
            Ok(closing) => closing,
            Err(error) => {
                prompt.abort();
                return Err(error);
            }
        };
        let mut losses = Vec::new();
        for loss in tokens {
            let weak = Arc::downgrade(&self.state);
            let cx = self.cx.clone();
            let worker = self.worker.clone();
            losses.push(loss.on_cancel(move || {
                let weak = weak.clone();
                let cx = cx.clone();
                let worker = worker.clone();
                crate::host::w::post_core(move |_, _| {
                    if let Some(state) = weak.upgrade() {
                        PasswordPrompt {
                            service: DocumentPasswordService { cx, state, worker },
                            id,
                        }
                        .abort();
                    }
                });
            }));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "The password service is unavailable")?;
        if let Some(scope) = state
            .owned
            .get_mut(&id)
            .filter(|scope| scope.authority.is_some())
        {
            scope.losses = losses;
            scope.closing = Some(closing);
        }
        drop(state);
        Ok(prompt)
    }

    fn retire_owned(&self, prompt: PromptId) {
        let retired = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.owned.get_mut(&prompt).map(|scope| {
                (
                    scope.authority.take(),
                    std::mem::take(&mut scope.losses),
                    scope.closing.take(),
                )
            })
        };
        drop(retired);
    }

    fn complete(&self, ask: AskId, answer: Result<Option<String>, String>) {
        let callback = self
            .state
            .lock()
            .expect("document password state")
            .take_answer(ask);
        if let Some(callback) = callback {
            callback(answer);
        }
    }

    fn effects(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Cancelled { ask } => {
                    let (callback, interrupted) = {
                        let mut state = self.state.lock().expect("document password state");
                        let interrupted = state
                            .callbacks
                            .get(&ask)
                            .is_some_and(|(id, _)| state.aborted_saved.contains(id));
                        (state.take_answer(ask), interrupted)
                    };
                    if let Some(callback) = callback {
                        callback(if interrupted {
                            Err("The saved password request was interrupted".into())
                        } else {
                            Ok(None)
                        });
                    }
                }
                Effect::CloseWindow { prompt } => {
                    let (callbacks, interrupted) = {
                        let mut state = self.state.lock().expect("document password state");
                        let interrupted = state.aborted_saved.contains(&prompt);
                        (state.cancel_answers(prompt), interrupted)
                    };
                    let key = self
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .owned
                        .get(&prompt)
                        .map(|scope| scope.key.clone());
                    let _ = on_core(move |provided| {
                        if let Some(tools) = provided.all::<PdfTools>().pop() {
                            if let Some(key) = key {
                                tools.close_password_dialog_owned(&key);
                            } else {
                                tools.close_password_dialog();
                            }
                        }
                    });
                    for callback in callbacks {
                        callback(if interrupted {
                            Err("The saved password request was interrupted".into())
                        } else {
                            Ok(None)
                        });
                    }
                }
                Effect::Prompt {
                    prompt,
                    ask,
                    file_name,
                    retry,
                } => {
                    let parent = self
                        .state
                        .lock()
                        .expect("document password state")
                        .parents
                        .get(&prompt)
                        .copied()
                        .flatten();
                    let service = self.clone();
                    if self.worker.send(Box::new(move || {
                        let lang = ShellLanguage(service.cx.ui.clone()).lang();
                        let state = super::strings::state(&file_name, retry, lang.as_str());
                        let callback_service = service.clone();
                        let available = on_core(move |provided| {
                            let scope = {
                                let state = callback_service.state.lock().expect("document password state");
                                if !state.callbacks.contains_key(&ask) { return true; }
                                match state.owned.get(&prompt) {
                                    Some(scope) => match &scope.authority {
                                        Some(authority) => Some(Ok((scope.key.clone(), authority.clone()))),
                                        None => Some(Err(())),
                                    },
                                    None => None,
                                }
                            };
                            if matches!(&scope, Some(Err(()))) {
                                callback_service.complete(ask, Ok(None));
                                return true;
                            }
                            let Some(tools) = provided.all::<PdfTools>().pop() else { return false; };
                            let completion = callback_service.clone();
                            match scope {
                                Some(Ok((key, PromptScope::Renderer(authority)))) => {
                                    tools.prompt_password_owned(parent, state, key, authority.source, authority.destination, authority.cancel, authority.pending,
                                        Box::new(move |answer| completion.complete(ask, answer)));
                                }
                                Some(Ok((key, PromptScope::Prepared { target, snapshot }))) => {
                                    let checked = target.clone();
                                    let checked_snapshot = snapshot.clone();
                                    tools.prompt_password_prepared(parent, state, key, target, snapshot, Box::new(move |answer| {
                                        let answer = crate::ipc::native_owner::with_recovery_seed_snapshot(&checked, &checked_snapshot, || answer)
                                            .unwrap_or_else(|| Err("The recovery request expired during password entry".into()));
                                        completion.complete(ask, answer);
                                    }));
                                }
                                Some(Ok((key, PromptScope::Saved { document, factory }))) => {
                                    let checked = document.clone();
                                    tools.prompt_password_saved(state, key, document, factory, Box::new(move |answer| {
                                        let target = checked.target();
                                        let mut answer = Some(answer);
                                        let mut admitted = None;
                                        let result = crate::ipc::native_owner::with_saved_document_effect(
                                            &checked, &[], target.cancel(), target.pending(), &[], &mut || {
                                                admitted = answer.take();
                                                Ok(())
                                            },
                                        );
                                        completion.complete(ask, match result {
                                            Ok(()) => admitted.unwrap_or_else(|| Err("The saved password answer was not admitted".into())),
                                            Err(error) => Err(error),
                                        });
                                    }));
                                }
                                Some(Err(())) => unreachable!("the retired password scope was handled above"),
                                None => tools.prompt_password(parent, state, Box::new(move |answer| completion.complete(ask, answer))),
                            }
                            true
                        });
                        if available != Some(true) {
                            service.complete(ask, Err("Password service is unavailable".into()));
                        }
                    })).is_err() {
                        self.complete(ask, Err("Password worker stopped".into()));
                    }
                }
            }
        }
    }
}

impl PasswordPrompt {
    pub(crate) fn ask(&self, retry: bool, callback: Callback) {
        let callback = Arc::new(Mutex::new(Some(callback)));
        let pending = callback.clone();
        let prompt = self.clone();
        let accepted = on_core(move |_| {
            let callback = pending
                .lock()
                .expect("password callback")
                .take()
                .expect("password callback present");
            let aborted = {
                let state = prompt
                    .service
                    .state
                    .lock()
                    .expect("document password state");
                (
                    state.aborted.contains(&prompt.id),
                    state.aborted_saved.contains(&prompt.id),
                )
            };
            if aborted.0 {
                callback(if aborted.1 {
                    Err("The saved password request was interrupted".into())
                } else {
                    Ok(None)
                });
                return;
            }
            let effects = {
                let mut state = prompt
                    .service
                    .state
                    .lock()
                    .expect("document password state");
                let (ask, effects) = state.model.ask(prompt.id, retry);
                state.callbacks.insert(ask, (prompt.id, callback));
                effects
            };
            prompt.service.effects(effects);
        });
        if accepted.is_none() {
            if let Some(callback) = callback.lock().expect("password callback").take() {
                callback(Err("Password service is unavailable".into()));
            }
        }
    }
    fn abort(&self) {
        let prompt = self.clone();
        let _ = on_core(move |_| {
            let effects = {
                let mut state = prompt
                    .service
                    .state
                    .lock()
                    .expect("document password state");
                if state.owned.get(&prompt.id).is_some_and(|scope| {
                    matches!(&scope.authority, Some(PromptScope::Saved { .. }))
                }) {
                    state.aborted_saved.insert(prompt.id);
                }
                state.aborted.insert(prompt.id);
                state.model.abort(prompt.id)
            };
            prompt.service.retire_owned(prompt.id);
            prompt.service.effects(effects);
        });
    }

    pub(crate) fn close(&self) {
        let prompt = self.clone();
        let _ = on_core(move |_| {
            let effects = prompt
                .service
                .state
                .lock()
                .expect("document password state")
                .model
                .close(prompt.id);
            prompt.service.retire_owned(prompt.id);
            prompt.service.effects(effects);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_one_prompt_settles_once_without_touching_the_next_prompt() {
        let mut state = State::default();
        let first = state.model.create("first.pptx");
        let second = state.model.create("second.xlsx");
        let (first_ask, _) = state.model.ask(first, false);
        let (second_ask, _) = state.model.ask(second, false);
        let answers = Arc::new(Mutex::new(Vec::new()));
        let first_answers = answers.clone();
        state.callbacks.insert(
            first_ask,
            (
                first,
                Box::new(move |answer| first_answers.lock().unwrap().push(("first", answer))),
            ),
        );
        let second_answers = answers.clone();
        state.callbacks.insert(
            second_ask,
            (
                second,
                Box::new(move |answer| second_answers.lock().unwrap().push(("second", answer))),
            ),
        );
        let callbacks = state.cancel_answers(first);
        for callback in callbacks {
            callback(Ok(None));
        }
        assert!(state.take_answer(first_ask).is_none());
        assert!(state.cancel_answers(first).is_empty());
        let second = state.take_answer(second_ask).unwrap();
        second(Ok(Some("second-password".into())));
        assert!(state.take_answer(second_ask).is_none());
        assert_eq!(
            *answers.lock().unwrap(),
            vec![
                ("first", Ok(None)),
                ("second", Ok(Some("second-password".into())))
            ]
        );
    }

    #[test]
    fn an_answer_arriving_before_close_is_never_cancelled_a_second_time() {
        let mut state = State::default();
        let prompt = state.model.create("protected.docx");
        let (ask, _) = state.model.ask(prompt, false);
        let answers = Arc::new(Mutex::new(Vec::new()));
        let observed = answers.clone();
        state.callbacks.insert(
            ask,
            (
                prompt,
                Box::new(move |answer| observed.lock().unwrap().push(answer)),
            ),
        );
        state.take_answer(ask).unwrap()(Ok(Some("correct".into())));
        assert!(state.cancel_answers(prompt).is_empty());
        assert_eq!(*answers.lock().unwrap(), vec![Ok(Some("correct".into()))]);
    }
}
