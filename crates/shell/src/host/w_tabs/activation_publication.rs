//! Publication of an immutable tab activation snapshot.
use main_core::shell::tab_manager::TabKind;
use shell_contract::{IpcValue, hosts::OwnedEffectsHost, ipc::NativeRequestOwner, runtime::{CancelToken, Views}};
use shell_ipc_w::tabs::{PreparedTabActivationPublication, TabActivationAuthority, TabActivationPublicationSnapshot};
use super::Hooks;
use tauri::Manager;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

type ReplySlot = std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>;
struct RetainedReply {
    slot: ReplySlot,
    indeterminate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    done: shell_ipc_w::tabs::TabUiDone,
}
thread_local! {
    static REPLIES: RefCell<std::collections::HashMap<u64, RetainedReply>> = RefCell::new(std::collections::HashMap::new());
}
static NEXT_REPLY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn drain_reply(id: u64) {
    let ready = REPLIES.with(|replies| {
        let mut replies = replies.borrow_mut();
        let result = replies.get(&id)?.slot.lock().unwrap_or_else(|error| error.into_inner()).take()?;
        let reply = replies.remove(&id)?;
        Some((reply.done, result))
    });
    if let Some((done, result)) = ready { done(result); }
}

pub(crate) fn retry_retained_replies() {
    let ids = REPLIES.with(|replies| replies.borrow().keys().copied().collect::<Vec<_>>());
    for id in ids { drain_reply(id); }
}

pub(crate) fn pending_retained_replies() -> usize {
    REPLIES.with(|replies| replies.borrow().len())
}

pub(crate) fn indeterminate_retained_replies() -> usize {
    REPLIES.with(|replies| replies.borrow().values()
        .filter(|reply| reply.indeterminate.load(std::sync::atomic::Ordering::Acquire)).count())
}

fn retained_reply(
    core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
    done: shell_ipc_w::tabs::TabUiDone,
) -> PendingReply {
    let id = NEXT_REPLY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let slot: ReplySlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let indeterminate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    REPLIES.with(|replies| { replies.borrow_mut().insert(id, RetainedReply {
        slot: slot.clone(), indeterminate: indeterminate.clone(), done,
    }); });
    let reply = Box::new(move |result| {
        *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(result);
        core.post(Box::new(move || drain_reply(id)));
    });
    PendingReply { reply: Some(reply), started: false, indeterminate }
}

struct PendingReply {
    reply: Option<Box<dyn FnOnce(Result<(), String>) + Send>>,
    started: bool,
    indeterminate: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl PendingReply {
    fn finish(mut self, result: Result<(), String>) {
        if let Some(reply) = self.reply.take() { reply(result); }
    }
}
impl Drop for PendingReply {
    fn drop(&mut self) {
        if !self.started {
            if let Some(reply) = self.reply.take() {
                reply(Err("The activation publication operation was discarded before execution.".into()));
            }
        } else if self.reply.is_some() {
            self.indeterminate.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

type PublicationStep = Rc<dyn Fn(shell_ipc_w::tabs::TabUiDone)>;
struct PublicationQueue { steps: VecDeque<PublicationStep>, running: bool }
impl PublicationQueue {
    fn new(steps: VecDeque<PublicationStep>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self { steps, running: false }))
    }
}

#[derive(Clone)]
struct PublicationAdmission {
    authority: TabActivationAuthority,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    effects: OwnedEffectsHost,
}
impl PublicationAdmission {
    fn run(&self, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        admit(&self.effects, &self.authority, &self.participants, &self.cancel, &self.pending, effect)
    }
    fn emit(&self, outlet: &crate::ipc::outlet::TauriOutlet, revision: &shell_ipc_w::tabs::SplitMemoryRevision,
        target: usize, channel: &str, args: Vec<IpcValue>) -> Result<(), EmitFailure> {
        emit(outlet, revision, &self.effects, &self.authority, &self.participants, target,
            &self.cancel, &self.pending, channel, args)
    }
}
fn sync_step(action: impl Fn() -> Result<(), String> + 'static) -> PublicationStep {
    Rc::new(move |done| done(action()))
}

enum EmitFailure { Rejected(String), Indeterminate(String) }
fn event_step(action: impl Fn() -> Result<(), EmitFailure> + 'static) -> PublicationStep {
    let uncertain = RefCell::new(None::<String>);
    sync_step(move || {
        if let Some(error) = uncertain.borrow().as_ref() { return Err(error.clone()); }
        match action() {
            Ok(()) => Ok(()),
            Err(EmitFailure::Rejected(error)) => Err(error),
            Err(EmitFailure::Indeterminate(error)) => {
                *uncertain.borrow_mut() = Some(error.clone());
                Err(error)
            }
        }
    })
}
fn session_step(snapshot: crate::host::recovery_launch::ActivationSessionSnapshot,
    admission: PublicationAdmission) -> PublicationStep {
    crate::host::recovery_launch::prepare_activation_session(
        snapshot, admission.authority, admission.participants, admission.cancel,
        admission.pending, admission.effects)
}

fn run_publication(queue: Rc<RefCell<PublicationQueue>>, done: shell_ipc_w::tabs::TabUiStepDone) {
    retry_retained_replies();
    let (running, step) = {
        let mut state = queue.borrow_mut();
        if state.running {
            (true, None)
        } else {
            let step = state.steps.front().cloned();
            if step.is_some() { state.running = true; }
            (false, step)
        }
    };
    if running {
        let retry = Rc::new(move |done| run_publication(queue.clone(), done));
        return done(Ok(shell_ipc_w::tabs::TabUiStepOutcome::CommittedCleanupPending {
            error: "Activation publication is already running".into(), retry,
        }));
    }
    let Some(step) = step else {
        return done(Ok(shell_ipc_w::tabs::TabUiStepOutcome::Applied));
    };
    step(Box::new(move |result| {
        {
            let mut state = queue.borrow_mut();
            state.running = false;
            if result.is_ok() { state.steps.pop_front(); }
        }
        match result {
            Ok(()) => run_publication(queue, done),
            Err(error) => {
                let retry = Rc::new(move |done| run_publication(queue.clone(), done));
                done(Ok(shell_ipc_w::tabs::TabUiStepOutcome::CommittedCleanupPending { error, retry }));
            }
        }
    }));
}

pub(super) fn admit(
    effects: &OwnedEffectsHost, authority: &TabActivationAuthority, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    match authority {
        TabActivationAuthority::Interactive(origin) => effects.with_owned_effect_many(origin, participants, cancel, pending, effect),
        TabActivationAuthority::NativeInvocation { document, owner } =>
            crate::ipc::native_owner::with_native_invocation_many_effect(
                document.target(), owner, participants, cancel, pending, effect)
                .ok_or_else(|| "The captured native invocation is no longer active.".to_owned())?,
        TabActivationAuthority::NativeOperation { operation, owner } =>
            effects.with_native_operation_effect(operation, owner, participants, cancel, pending, effect),
        TabActivationAuthority::StartupRecovery { startup, shell, child, owner } =>
            effects.with_startup_recovery_effect(startup, shell, child, owner, participants, cancel, pending, effect),
        TabActivationAuthority::NativeCommand { operation, expected, owner } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            effects.with_native_command_preparation_effect(operation, expected, &captured, cancel, pending, effect)
        }
        TabActivationAuthority::NativeCommandTarget { document, expected, owner } =>
            effects.with_native_command_target_effect(document.operation(), expected, document, owner, participants, cancel, pending, effect),
        TabActivationAuthority::StartupCommand { startup, shell, child, expected, owner } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            effects.with_startup_command_preparation_effect(startup, shell, child, expected, &captured, cancel, pending, effect)
        }
        TabActivationAuthority::StartupCommandTarget { startup, shell, document, expected, owner } =>
            effects.with_startup_command_target_effect(startup, shell, document.operation(), expected, document, owner, participants, cancel, pending, effect),
        TabActivationAuthority::CommandRecovery { command, child, owner } =>
            effects.with_command_recovery_target_effect(command, child, owner, participants, cancel, pending, effect),
        TabActivationAuthority::ExistingDocumentSave { path, owner, additional } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            let extra: Vec<_> = additional.iter().collect();
            path.with_effect(effects, &captured, cancel, pending, &extra, effect)
        }
        TabActivationAuthority::SavedDocument { document, owner, additional } => {
            let extra: Vec<_> = additional.iter().collect();
            effects.with_native_saved_document_same_origin(document, owner, cancel, participants, cancel, pending, &extra, &mut |_| effect())
        }
        TabActivationAuthority::NativeSaveReload { receipt, owner } =>
            effects.with_native_save_reload_many_effect(receipt, owner, participants, cancel, pending, effect),
    }
}

fn emit(
    outlet: &crate::ipc::outlet::TauriOutlet, revision: &shell_ipc_w::tabs::SplitMemoryRevision,
    effects: &OwnedEffectsHost, authority: &TabActivationAuthority, participants: &[NativeRequestOwner],
    target: usize, cancel: &CancelToken, pending: &CancelToken, channel: &str, args: Vec<IpcValue>,
) -> Result<(), EmitFailure> {
    let target = participants.get(target).ok_or_else(|| EmitFailure::Rejected("The captured event target is unavailable.".into()))?;
    let generation = target.context.renderer().ok_or_else(|| EmitFailure::Rejected("The captured event renderer is unavailable.".into()))?;
    let mut frame = Some(shell_core::outlet::event_frame(channel, &args));
    let mut drain = None;
    admit(effects, authority, participants, cancel, pending, &mut || {
        revision.with_current_commit(|| {
            let frame = frame.take().ok_or("The captured event was already enqueued.")?;
            drain = outlet.enqueue_generation_fenced(target.view, generation, frame)?;
            Ok(())
        }).ok_or("The captured split state was superseded.")?
    }).map_err(EmitFailure::Rejected)?;
    if let Some(drain) = drain { drain.drain().map_err(EmitFailure::Indeterminate)?; }
    Ok(())
}

pub(super) fn prepare(
    hooks: &Hooks, origin: TabActivationAuthority, shell: NativeRequestOwner,
    mut participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
    effects: OwnedEffectsHost, snapshot: TabActivationPublicationSnapshot,
) -> Result<PreparedTabActivationPublication, String> {
    participants.push(shell.clone());
    let outlet = hooks.cx.app.try_state::<crate::ipc::Ipc>().ok_or("IPC output is not ready.")?.outlet.clone();
    let changed_outlet = outlet.clone();
    let changed_revision = snapshot.split_memory_revision.clone();
    let split_revision = snapshot.split_memory_revision.clone();
    admit(&effects, &origin, &participants, &cancel, &pending, &mut || Ok(()))?;
    let shell_index = participants.len() - 1;
    let window = hooks.services.views.sender_of(shell.view).ok_or("activation shell is unavailable")?.window;
    let mut recipients = Vec::new();
    for (id, view) in &snapshot.tab_views {
        let index = participants.iter().position(|owner| owner.view == *view)
            .ok_or("activation tab owner was not captured")?;
        recipients.push((id.clone(), *view, index));
    }
    let mut hwp_targets = Vec::new();
    for tab in snapshot.changed.tabs.iter().filter(|tab| tab.kind == TabKind::Hwp) {
        let Some((_, view, index)) = recipients.iter().find(|(id, _, _)| id == &tab.id) else { continue };
        let editor = shell_contract::editors::editor_for(&hooks.provided, *view).ok_or("HWP editor unavailable")?;
        let owner = editor.capture_document_owner(*view, participants[*index].clone()).map_err(|error| error.to_string())?;
        hwp_targets.push((*view, tab.title.clone(), owner));
    }
    let hwp = hooks.provided.all::<shell_ipc_d2::hwp::HwpEditors>().into_iter().next();
    if !hwp_targets.is_empty() && hwp.is_none() { return Err("HWP recovery service unavailable".into()); }
    let changed_session = crate::host::recovery_launch::capture_activation_session(&snapshot.session_strip, &snapshot.session_detached, &snapshot.tab_views)?;
    let split_session = crate::host::recovery_launch::capture_activation_session(&snapshot.session_strip, &snapshot.session_detached, &snapshot.tab_views)?;
    let active = snapshot.changed.tabs.iter().find(|tab| tab.active);
    let menu = if let Some(tab) = active {
        let view = recipients.iter().find(|(id, _, _)| id == &tab.id).map(|(_, view, _)| *view);
        Some(crate::host::menu::prepare_activation_menu(&hooks.cx.app, tab.kind, view, &hooks.provided, &hooks.services)?)
    } else { None };
    let changed_args = snapshot.changed_args();
    let split_args = snapshot.split_args();
    let ids: Vec<_> = snapshot.changed.tabs.iter().map(|tab| tab.id.clone()).collect();
    let split_tab_ids = ids.clone();
    let split_state = snapshot.split.clone();
    let core = hooks.services.core.clone();
    let ui = hooks.services.ui.clone();
    let admission = PublicationAdmission { authority: origin.clone(), participants: participants.clone(),
        cancel: cancel.clone(), pending: pending.clone(), effects: effects.clone() };
    let mut changed_steps = VecDeque::new();
    {
        let admission = admission.clone();
        changed_steps.push_back(event_step(move || admission.emit(&changed_outlet, &changed_revision, shell_index, "tabs:changed", changed_args.clone())));
    }
    {
        let admission = admission.clone();
        changed_steps.push_back(sync_step(move || admission.run(&mut || {
            crate::host::recovery_openers::retain_bound(window, &ids);
            Ok(())
        })));
    }
    if let Some(hwp) = hwp {
        for (view, title, owner) in hwp_targets {
            let hwp = hwp.clone();
            let mut admission = admission.clone();
            admission.participants.push(owner);
            changed_steps.push_back(sync_step(move || admission.run(&mut || {
                hwp.set_recovery_title(view, &title);
                Ok(())
            })));
        }
    }
    if let Some(menu) = menu {
        let admission = admission.clone();
        changed_steps.push_back(Rc::new(move |done| {
            let mut reply = retained_reply(core.clone(), done);
            let current_ui = ui.clone();
            let admission = admission.clone();
            let menu = menu.clone();
            ui.post(Box::new(move || {
                reply.started = true;
                let result = super::activation::run_accepted_ui(current_ui.as_ref(), &admission.effects,
                    &admission.authority, &admission.participants, &admission.cancel, &admission.pending,
                    Box::new(move || menu())).and_then(|result| result);
                reply.finish(result);
            }));
        }));
    }
    changed_steps.push_back(session_step(changed_session, admission.clone()));
    let changed = Box::new(move |done| run_publication(PublicationQueue::new(changed_steps), done));

    let split_admission = admission.clone();
    let split = Box::new(move |ratio_only: bool, done| {
        let mut steps = VecDeque::new();
        if !ratio_only {
            for (id, _, index) in &recipients {
                if !split_tab_ids.iter().any(|tab_id| tab_id == id) { continue; }
                let role = split_state.as_ref().and_then(|state| {
                    let pane = if id == &state.a_tab_id { "a" } else if id == &state.b_tab_id { "b" } else { return None };
                    Some(if state.focus.as_str() == pane { "focus" } else { "secondary" })
                });
                let args = vec![role.map_or(IpcValue::Null, |role| IpcValue::String(role.into()))];
                let outlet = outlet.clone();
                let revision = split_revision.clone();
                let admission = split_admission.clone();
                let index = *index;
                steps.push_back(event_step(move || admission.emit(&outlet, &revision, index, "app:split-role-changed", args.clone())));
            }
        }
        {
            let admission = split_admission.clone();
            steps.push_back(event_step(move || admission.emit(&outlet, &split_revision, shell_index, "tabs:split-changed", split_args.clone())));
        }
        steps.push_back(session_step(split_session, split_admission));
        run_publication(PublicationQueue::new(steps), done);
    });
    let memory = snapshot.split.as_ref().map(|state| {
        let path = |id: &str| snapshot.changed.tabs.iter().find(|tab| tab.id == id).and_then(|tab| tab.file_path.clone());
        main_core::shell::split_memory::SplitMemoryState {
            a_tab_id_path: path(&state.a_tab_id), b_tab_id_path: path(&state.b_tab_id),
            orientation: Some(state.orientation.as_str().into()), ratio: Some(state.ratio), focus: Some(state.focus.as_str().into()),
        }
    });
    let path = snapshot.settings_path;
    let revision = snapshot.split_memory_revision;
    let workers_core = hooks.services.core.clone();
    let persist_step: PublicationStep = Rc::new(move |done| {
        if let Err(error) = admit(&effects, &origin, &participants, &cancel, &pending, &mut || Ok(())) {
            done(Err(error));
            return;
        }
        let mut reply = retained_reply(workers_core.clone(), done);
        let path = path.clone();
        let memory = memory.clone();
        let revision = revision.clone();
        let effects = effects.clone();
        let origin = origin.clone();
        let participants = participants.clone();
        let cancel = cancel.clone();
        let pending = pending.clone();
        main_core::shell::app_settings::queue::for_path(&path).submit(move |completion| {
            reply.started = true;
            if let Err(error) = admit(&effects, &origin, &participants, &cancel, &pending, &mut || Ok(())) {
                drop(completion);
                reply.finish(Err(error));
                return;
            }
            let write = |path: &str, key: &str, value| {
                main_core::shell::app_settings::write_app_setting_guarded(path, key, value, |temp, target| {
                    let mut outcome = None;
                    admit(&effects, &origin, &participants, &cancel, &pending, &mut || {
                        outcome = revision.with_current_commit(|| std::fs::rename(temp, target)).map(Some);
                        Ok(())
                    }).map_err(|message| main_core::node_fs::NodeError { code: None, message })?;
                    match outcome.flatten() {
                        Some(result) => result.map_err(|error| main_core::node_fs::node_error2(&error, "rename", temp, target)),
                        None => Err(main_core::node_fs::NodeError { code: None, message: "split memory was superseded".into() }),
                    }
                })
            };
            let options = main_core::shell::split_memory::SplitMemoryOptions {
                settings_path: main_core::shell::split_memory::SettingsPath::Fixed(path), read_settings: None, write_setting: Some(&write),
            };
            let result = main_core::shell::split_memory::write_split_memory(memory.as_ref(), &options).map_err(|error| error.message);
            drop(completion);
            reply.finish(result);
        });
    });
    let persist_split = Box::new(move |done| run_publication(PublicationQueue::new(VecDeque::from([persist_step])), done));
    Ok(PreparedTabActivationPublication { changed, split, persist_split })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct DroppedCore;
    impl shell_contract::runtime::Thread for DroppedCore {
        fn post(&self, _job: Box<dyn FnOnce() + Send>) {}
        fn is_current(&self) -> bool { false }
    }

    #[test]
    fn retained_async_result_survives_a_dropped_core_wake() {
        let results = Rc::new(RefCell::new(Vec::new()));
        let output = results.clone();
        let reply = retained_reply(std::sync::Arc::new(DroppedCore), Box::new(move |result| {
            output.borrow_mut().push(result);
        }));
        reply.finish(Err("original result".into()));
        assert!(results.borrow().is_empty());
        retry_retained_replies();
        assert_eq!(&*results.borrow(), &[Err("original result".into())]);
        retry_retained_replies();
        assert_eq!(results.borrow().len(), 1);
    }

    #[test]
    fn discarded_before_execution_reports_retry_without_running_the_effect() {
        let results = Rc::new(RefCell::new(Vec::new()));
        let output = results.clone();
        let reply = retained_reply(std::sync::Arc::new(DroppedCore), Box::new(move |result| {
            output.borrow_mut().push(result);
        }));
        drop(reply);
        assert!(results.borrow().is_empty());
        retry_retained_replies();
        assert_eq!(results.borrow().len(), 1);
        assert!(results.borrow()[0].is_err());
    }

    #[test]
    fn started_without_a_result_remains_explicitly_indeterminate() {
        let mut reply = retained_reply(std::sync::Arc::new(DroppedCore), Box::new(|_| panic!("no result")));
        reply.started = true;
        drop(reply);
        assert_eq!(pending_retained_replies(), 1);
        assert_eq!(indeterminate_retained_replies(), 1);
        retry_retained_replies();
        assert_eq!(pending_retained_replies(), 1);
        REPLIES.with(|replies| replies.borrow_mut().clear());
    }

    #[test]
    fn retry_drains_the_retained_result_without_reexecuting_the_effect() {
        let calls = Rc::new(Cell::new(0));
        let step: PublicationStep = {
            let calls = calls.clone();
            Rc::new(move |done| {
                calls.set(calls.get() + 1);
                retained_reply(std::sync::Arc::new(DroppedCore), done).finish(Ok(()));
            })
        };
        let queue = PublicationQueue::new(VecDeque::from([step]));
        let first = Rc::new(RefCell::new(None));
        let result = first.clone();
        run_publication(queue.clone(), Box::new(move |value| *result.borrow_mut() = Some(value)));
        assert!(first.borrow().is_none());
        let second = Rc::new(RefCell::new(None));
        let result = second.clone();
        run_publication(queue, Box::new(move |value| *result.borrow_mut() = Some(value)));
        assert!(matches!(first.borrow_mut().take().unwrap(), Ok(shell_ipc_w::tabs::TabUiStepOutcome::Applied)));
        assert!(matches!(second.borrow_mut().take().unwrap(), Ok(shell_ipc_w::tabs::TabUiStepOutcome::Applied)));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn retry_resumes_only_the_first_unfinished_effect() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let attempts = Rc::new(Cell::new(0));
        let first = {
            let log = log.clone();
            sync_step(move || { log.borrow_mut().push("first"); Ok(()) })
        };
        let failing = {
            let log = log.clone();
            let attempts = attempts.clone();
            sync_step(move || {
                attempts.set(attempts.get() + 1);
                log.borrow_mut().push("second");
                if attempts.get() == 1 { Err("temporary failure".into()) } else { Ok(()) }
            })
        };
        let last = {
            let log = log.clone();
            sync_step(move || { log.borrow_mut().push("last"); Ok(()) })
        };
        let outcome = Rc::new(RefCell::new(None));
        let result = outcome.clone();
        run_publication(PublicationQueue::new(VecDeque::from([first, failing, last])),
            Box::new(move |value| *result.borrow_mut() = Some(value)));
        let shell_ipc_w::tabs::TabUiStepOutcome::CommittedCleanupPending { error, retry } =
            outcome.borrow_mut().take().unwrap().unwrap() else { panic!("expected retry") };
        assert_eq!(error, "temporary failure");
        assert_eq!(&*log.borrow(), &["first", "second"]);
        let result = outcome.clone();
        retry(Box::new(move |value| *result.borrow_mut() = Some(value)));
        assert!(matches!(outcome.borrow_mut().take().unwrap(), Ok(shell_ipc_w::tabs::TabUiStepOutcome::Applied)));
        assert_eq!(&*log.borrow(), &["first", "second", "second", "last"]);
        let result = outcome.clone();
        retry(Box::new(move |value| *result.borrow_mut() = Some(value)));
        assert!(matches!(outcome.borrow_mut().take().unwrap(), Ok(shell_ipc_w::tabs::TabUiStepOutcome::Applied)));
        assert_eq!(&*log.borrow(), &["first", "second", "second", "last"]);
    }

    #[test]
    fn concurrent_retry_never_replays_in_flight_effect() {
        let pending: Rc<RefCell<Option<shell_ipc_w::tabs::TabUiDone>>> = Rc::default();
        let calls = Rc::new(Cell::new(0));
        let step: PublicationStep = {
            let pending = pending.clone();
            let calls = calls.clone();
            Rc::new(move |done| { calls.set(calls.get() + 1); *pending.borrow_mut() = Some(done); })
        };
        let queue = PublicationQueue::new(VecDeque::from([step]));
        let first = Rc::new(RefCell::new(None));
        let output = first.clone();
        run_publication(queue.clone(), Box::new(move |result| *output.borrow_mut() = Some(result)));
        let second = Rc::new(RefCell::new(None));
        let output = second.clone();
        run_publication(queue, Box::new(move |result| *output.borrow_mut() = Some(result)));
        let shell_ipc_w::tabs::TabUiStepOutcome::CommittedCleanupPending { retry, .. } =
            second.borrow_mut().take().unwrap().unwrap() else { panic!("expected pending retry") };
        assert_eq!(calls.get(), 1);
        pending.borrow_mut().take().unwrap()(Ok(()));
        assert!(matches!(first.borrow_mut().take().unwrap(), Ok(shell_ipc_w::tabs::TabUiStepOutcome::Applied)));
        let output = second.clone();
        retry(Box::new(move |result| *output.borrow_mut() = Some(result)));
        assert!(matches!(second.borrow_mut().take().unwrap(), Ok(shell_ipc_w::tabs::TabUiStepOutcome::Applied)));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn indeterminate_event_is_reported_without_reenqueue_on_retry() {
        let attempts = Rc::new(Cell::new(0));
        let step = {
            let attempts = attempts.clone();
            event_step(move || {
                attempts.set(attempts.get() + 1);
                Err(EmitFailure::Indeterminate("delivery uncertain".into()))
            })
        };
        let outcome = Rc::new(RefCell::new(None));
        let result = outcome.clone();
        run_publication(PublicationQueue::new(VecDeque::from([step])),
            Box::new(move |value| *result.borrow_mut() = Some(value)));
        let shell_ipc_w::tabs::TabUiStepOutcome::CommittedCleanupPending { error, retry } =
            outcome.borrow_mut().take().unwrap().unwrap() else { panic!("expected indeterminate failure") };
        assert_eq!(error, "delivery uncertain");
        let result = outcome.clone();
        retry(Box::new(move |value| *result.borrow_mut() = Some(value)));
        let shell_ipc_w::tabs::TabUiStepOutcome::CommittedCleanupPending { error, .. } =
            outcome.borrow_mut().take().unwrap().unwrap() else { panic!("expected unresolved delivery") };
        assert_eq!(error, "delivery uncertain");
        assert_eq!(attempts.get(), 1);
    }
}
