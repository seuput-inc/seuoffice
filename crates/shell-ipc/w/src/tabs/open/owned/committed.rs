use super::*;
use shell_contract::hosts::{
    GeneratedPathOutcome, GeneratedPathPending, GeneratedPathWake, OwnedEffectsHost,
    PdfReloadAccepted,
};
use shell_contract::ipc::{NativeDriveReopen, NativeSavedInput, NativeSavedPath, SavedPublication};
use std::sync::Arc;

pub(crate) type SavedReveal =
    Box<dyn FnOnce(CancelToken, Box<dyn FnOnce(Result<GeneratedPathOutcome, String>) + Send>)>;

pub struct PreparedGeneratedOperation {
    task: Arc<dyn GeneratedPathPending>,
    operation: Arc<super::pending_output::PendingOutput>,
    start: Option<Box<dyn FnOnce()>>,
}
impl shell_contract::hosts::PreparedGeneratedPath for PreparedGeneratedOperation {
    fn task(&self) -> Arc<dyn GeneratedPathPending> {
        self.task.clone()
    }
    fn start(mut self: Box<Self>) -> Result<(), shell_contract::hosts::GeneratedStartFailure> {
        if let Err(error) = self.operation.ready_to_start() {
            return Err(shell_contract::hosts::GeneratedStartFailure {
                error,
                prepared: self,
            });
        }
        if let Some(start) = self.start.take() {
            start();
        }
        Ok(())
    }
}
impl Drop for PreparedGeneratedOperation {
    fn drop(&mut self) {
        if self.start.is_some() {
            self.operation
                .finish(Err("The prepared generated operation was cancelled".into()));
        }
    }
}
thread_local! {
    static RETAINED_OUTPUTS: RefCell<HashMap<u64, Option<std::rc::Rc<dyn std::any::Any>>>> = RefCell::new(HashMap::new());
}
static NEXT_RETAINED_OUTPUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct RetainedKey {
    core: Arc<dyn shell_contract::runtime::Thread>,
    id: u64,
}
impl Drop for RetainedKey {
    fn drop(&mut self) {
        let id = self.id;
        self.core.post(Box::new(move || {
            RETAINED_OUTPUTS.with(|entries| {
                entries.borrow_mut().remove(&id);
            });
        }));
    }
}

struct ReopenCompletion {
    pending: bool,
    operation: Arc<super::pending_output::PendingOutput>,
}
impl Drop for ReopenCompletion {
    fn drop(&mut self) {
        if self.pending {
            self.operation.mark_indeterminate(
                "The Drive reopen discarded its completion; cleanup remains registered".into(),
            );
        }
    }
}

fn prompt_pending(
    prompt: std::rc::Rc<dyn crate::tabs::host::CapturedGeneratedRetryPrompt>,
    task: Arc<dyn GeneratedPathPending>,
    core: Arc<dyn shell_contract::runtime::Thread>,
    error: String,
) {
    let next_prompt = prompt.clone();
    prompt.show(
        error,
        Box::new(move |choice| {
            let callback_task = task.clone();
            let callback =
                crate::home_files::back_to_core(core.clone(), move |result: Result<(), String>| {
                    if let Err(error) = result {
                        prompt_pending(next_prompt, callback_task, core, error);
                    }
                });
            match choice {
                Ok(crate::tabs::host::GeneratedRetryChoice::Retry) => task.retry(callback),
                Ok(crate::tabs::host::GeneratedRetryChoice::Cancel) | Err(_) => {
                    task.cancel_and_drain(callback)
                }
            }
        }),
    );
}

struct RetainedOutput {
    _output: SavedPublication,
    _input: Option<NativeSavedInput>,
    _shell: std::rc::Rc<dyn crate::tabs::host::CapturedSavedShell>,
    _shell_window: crate::tabs::host::ShellWindow,
    _shell_owner: NativeRequestOwner,
    _manager_epoch: u64,
    _accepted: PdfReloadAccepted,
    _participants: Vec<NativeRequestOwner>,
    _cancel: CancelToken,
    _pending: CancelToken,
    _additional: Vec<CancelToken>,
    _effects: OwnedEffectsHost,
    _reveal: Option<SavedReveal>,
    _inverse: Arc<dyn shell_contract::ipc::NativeDrivePendingReceipt>,
    _key: Arc<RetainedKey>,
}

impl TabsState {
    /// Continue an actual committed publication. Drive and retained-object I/O
    /// happen off core; neither a selected path nor a successful save alone
    /// grants a new editor authority.
    pub(crate) fn prepare_committed_output_owned(
        &self,
        output: SavedPublication,
        written: Option<NativeSavedInput>,
        mut participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        accepted: PdfReloadAccepted,
        reveal: Option<SavedReveal>,
        reveal_only: bool,
        wake: GeneratedPathWake,
    ) -> Result<PreparedGeneratedOperation, String> {
        if self
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .is_none_or(|installed| !Arc::ptr_eq(installed, &effects))
        {
            return Err("The captured saved-output authority is unavailable".into());
        }
        if written
            .as_ref()
            .is_some_and(|input| !input.output().same_publication(&output))
        {
            return Err("The written input does not belong to the committed output".into());
        }
        let extra: Vec<_> = additional.iter().collect();
        if let Err(error) = output.with_effect(
            effects.as_ref(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || Ok(()),
        ) {
            return Err(error);
        }
        let shell = self
            .shell_window()
            .ok_or("The shell window is unavailable")?;
        let generation = match self.services.views.renderer_generation(shell.view) {
            Some(generation) => generation,
            None => return Err("The shell renderer is unavailable".into()),
        };
        let shell_owner = match effects.capture_owner(shell.view, &generation) {
            Ok(owner) => owner,
            Err(error) => return Err(error),
        };
        let captured_shell = match self.hooks.capture_saved_shell(shell, shell_owner.clone()) {
            Ok(captured) => captured,
            Err(error) => return Err(error),
        };
        participants.push(shell_owner.clone());
        let prompt = match self.hooks.capture_generated_retry_prompt(
            shell,
            shell_owner.clone(),
            output.clone(),
            participants.clone(),
            cancel.clone(),
            pending.clone(),
            additional.clone(),
        ) {
            Ok(prompt) => prompt,
            Err(error) => return Err(error),
        };
        let retained_id = NEXT_RETAINED_OUTPUT
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |id| id.checked_add(1),
            )
            .map_err(|_| "The generated cleanup identity is unavailable")?;
        let operation = super::pending_output::PendingOutput::new(wake);
        let task: Arc<dyn GeneratedPathPending> = Arc::new(operation.clone());
        let registration = match self.hooks.register_generated_path_task(task.clone()) {
            Ok(registration) => registration,
            Err(error) => return Err(error),
        };
        operation.install_registration(registration)?;
        let mut additional = additional;
        additional.push(operation.cancellation());
        let prepared_task = task.clone();
        let prepared_operation = operation.clone();
        let state_rc = self.rc();
        let start = Box::new(move || {
            if operation.shutting_down() {
                return operation.finish(Err("The generated operation was cancelled".into()));
            }
            if reveal_only {
                let Some(reveal) = reveal else {
                    return operation
                        .finish(Err("The captured reveal destination is unavailable".into()));
                };
                if let Err(error) = operation.begin_open() {
                    return operation.finish(Err(error));
                }
                let completed = operation.clone();
                return reveal(
                    operation.cancellation(),
                    Box::new(move |result| completed.finish(result)),
                );
            }
            let core = state_rc.services.core.clone();
            let epoch = state_rc.manager_epoch.get();
            let state = state_rc.this.clone();
            let next_output = output.clone();
            let next_participants = participants.clone();
            let next_cancel = cancel.clone();
            let next_pending = pending.clone();
            let next_additional = additional.clone();
            let mut completion = ReopenCompletion {
                pending: true,
                operation: operation.clone(),
            };
            state_rc.hooks.prepare_reopen_existing_document(
                output,
                participants,
                cancel,
                pending,
                additional,
                Box::new(move |result| {
                    completion.pending = false;
                    let path = match result {
                        Ok(NativeDriveReopen::CleanupPending(receipt)) => {
                            let error = receipt
                                .cleanup_error()
                                .unwrap_or_else(|| receipt.original_error().to_owned());
                            let key = Arc::new(RetainedKey {
                                core: core.clone(),
                                id: retained_id,
                            });
                            let retained: std::rc::Rc<dyn std::any::Any> =
                                std::rc::Rc::new(RetainedOutput {
                                    _output: next_output,
                                    _input: written,
                                    _shell: captured_shell,
                                    _shell_window: shell,
                                    _shell_owner: shell_owner,
                                    _manager_epoch: epoch,
                                    _accepted: accepted,
                                    _participants: next_participants,
                                    _cancel: next_cancel,
                                    _pending: next_pending,
                                    _additional: next_additional,
                                    _effects: effects,
                                    _reveal: reveal,
                                    _inverse: receipt.clone(),
                                    _key: key.clone(),
                                });
                            RETAINED_OUTPUTS.with(|entries| {
                                entries
                                    .borrow_mut()
                                    .insert(retained_id, Some(retained.clone()));
                            });
                            let release = Arc::new(move |done| {
                                let id = key.id;
                                key.core.post(Box::new(move || {
                                    let result = RETAINED_OUTPUTS.with(|entries| {
                                        let mut entries = entries.borrow_mut();
                                        let slot = entries.get_mut(&id).ok_or(
                                            "The captured generated payload is unavailable",
                                        )?;
                                        slot.take();
                                        Ok(())
                                    });
                                    done(result.map_err(str::to_owned));
                                }));
                            });
                            let shutting = match operation.retain_inverse(receipt, release) {
                                Ok(shutting) => shutting,
                                Err(error) => {
                                    operation.mark_indeterminate(error);
                                    return;
                                }
                            };
                            if shutting {
                                task.cancel_and_drain(Box::new(move |result| {
                                    if let Err(error) = result {
                                        eprintln!(
                                            "[shell] generated cleanup remains pending: {error}"
                                        );
                                    }
                                }));
                            } else {
                                prompt_pending(prompt, task, core, error);
                            }
                            return;
                        }
                        Err(error) => return operation.finish(Err(error)),
                        Ok(NativeDriveReopen::Unchanged(publication)) => {
                            NativeSavedPath::Selected(publication)
                        }
                        Ok(NativeDriveReopen::Moved(receipt)) => {
                            NativeSavedPath::DriveMoved(receipt)
                        }
                    };
                    let Some(state) = state.upgrade() else {
                        return operation.finish(Err("The tab manager is unavailable".into()));
                    };
                    if state.manager_epoch.get() != epoch || state.shell_window() != Some(shell) {
                        return operation.finish(Err("The captured shell window changed".into()));
                    }
                    if let Err(error) = operation.begin_open() {
                        return operation.finish(Err(error));
                    }
                    let input = written.filter(|_| matches!(&path, NativeSavedPath::Selected(_)));
                    let resume_state = state.this.clone();
                    let read_path = path.clone();
                    let read_participants = next_participants.clone();
                    let read_cancel = next_cancel.clone();
                    let read_pending = next_pending.clone();
                    let read_additional = next_additional.clone();
                    let fallback_core = state.services.core.clone();
                    let run = move |input| {
                        let Some(state) = resume_state.upgrade() else {
                            return operation.finish(Err("The tab manager is unavailable".into()));
                        };
                        match input {
                            Ok(input) => {
                                let completed = operation.clone();
                                let finish =
                                    crate::home_files::back_to_core(fallback_core, move |result| {
                                        match (result, reveal) {
                                            (Ok(GeneratedPathOutcome::NotOpened), Some(reveal)) => {
                                                let token = completed.cancellation();
                                                reveal(
                                                    token,
                                                    Box::new(move |result| {
                                                        completed.finish(result)
                                                    }),
                                                );
                                            }
                                            (result, _) => completed.finish(result),
                                        }
                                    });
                                state.open_saved_input_owned(
                                    input,
                                    path,
                                    shell,
                                    shell_owner,
                                    captured_shell,
                                    epoch,
                                    next_participants,
                                    next_cancel,
                                    next_pending,
                                    next_additional,
                                    effects,
                                    accepted,
                                    finish,
                                );
                            }
                            Err(error) => operation.finish(Err(error)),
                        }
                    };
                    if let Some(input) = input {
                        run(Ok(input));
                    } else {
                        state.hooks.read_saved_input(
                            next_output,
                            read_path,
                            read_participants,
                            read_cancel,
                            read_pending,
                            read_additional,
                            Box::new(run),
                        );
                    }
                }),
            );
        });
        Ok(PreparedGeneratedOperation {
            task: prepared_task,
            operation: prepared_operation,
            start: Some(start),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::hosts::{GeneratedCleanupRegistration, PreparedGeneratedPath};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Registration;
    impl GeneratedCleanupRegistration for Registration {
        fn complete(self: Box<Self>) {}
    }

    #[test]
    fn start_requires_preinstalled_observer_and_returns_same_preparation() {
        let operation = super::super::pending_output::PendingOutput::new(Arc::new(|| {}));
        operation
            .install_registration(Box::new(Registration))
            .unwrap();
        let task: Arc<dyn GeneratedPathPending> = Arc::new(operation.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let prepared: Box<dyn PreparedGeneratedPath> = Box::new(PreparedGeneratedOperation {
            task: task.clone(),
            operation,
            start: Some(Box::new(move || {
                invoked.fetch_add(1, Ordering::SeqCst);
            })),
        });
        let failure = match prepared.start() {
            Err(failure) => failure,
            Ok(()) => panic!("started without an observer"),
        };
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(Arc::ptr_eq(&task, &failure.prepared.task()));
        assert!(task.observe(Box::new(|_| {}), Arc::new(|| {})).is_ok());
        assert!(failure.prepared.start().is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
