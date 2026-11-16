//! Launch recovery orchestration on the core thread.
mod actions;
pub(super) mod issuer;
mod reveal;
mod session_writer;
mod worker;
pub(crate) use issuer::{RecoveryIssuer, StartupIssuer};

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use main_core::shell::recovery_shell::{
    LaunchDeps, LaunchMachine, LaunchNeed, LaunchReply, LaunchStep, RecoveryEditors, RecoveryShell,
    RestoredTabOpenerDeps,
};
use main_core::shell::renderer_disposal::Failure;
use main_core::shell::session_record::SessionRecordProcess;
use shell_ipc_u::launch_list::LaunchList;

use super::recovery_editors::Editors;
use super::recovery_manager::{Contents, Manager, SessionEnv, SharedStore};

struct LaunchInputs {
    manager: Manager,
    editors: Rc<Editors>,
    store: SharedStore,
    gate: Rc<LaunchList>,
}

fn capture_launch_inputs(
    services: &shell_contract::runtime::Services,
    provided: Rc<shell_contract::Provided>,
    cx: &super::HostContext,
    owner: shell_contract::WindowId,
    issuer: RecoveryIssuer,
    startup: shell_contract::ipc::StartupRecoveryConstraint,
) -> Result<LaunchInputs, String> {
    let tabs = provided
        .all::<shell_ipc_w::tabs::TabsState>()
        .into_iter()
        .next()
        .ok_or("the shell tab manager is unavailable")?;
    let gate =
        shell_ipc_u::launch_list::current().ok_or("the recovery launch list is unavailable")?;
    let shared = services
        .hosts
        .get::<std::sync::Arc<
            std::sync::Mutex<
                main_core::packages::electron_utils::recovery_store::SharedRecoveryState,
            >,
        >>()
        .cloned()
        .ok_or("the shared recovery store is unavailable")?;
    let path = services
        .paths
        .path(shell_contract::runtime::PathName::UserData);
    let user_data = path
        .to_str()
        .ok_or("the recovery user-data path is not valid UTF-8")?
        .to_owned();
    let store = SharedStore { shared, user_data };
    let (open, needs_user) = super::recovery_openers::live_callbacks(
        services.clone(),
        provided.clone(),
        gate.clone(),
        store.shared.clone(),
        store.user_data.clone(),
    );
    let manager = Manager {
        tabs,
        views: cx.views.clone(),
        owner,
        open,
        needs_user,
        issuer: issuer.clone(),
    };
    let editors = Rc::new(Editors {
        services: services.clone(),
        provided,
        issuer,
        owner,
        startup: Some(startup),
    });
    Ok(LaunchInputs {
        manager,
        editors,
        store,
        gate,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_native_launch(
    services: shell_contract::runtime::Services,
    provided: Rc<shell_contract::Provided>,
    cx: super::HostContext,
    shell_window: shell_contract::WindowId,
    shell_owner: shell_contract::ipc::NativeRequestOwner,
    startup: shell_contract::ipc::NativeInvocationOperation,
    recovery_issuer: RecoveryIssuer,
    startup_issuer: StartupIssuer,
    done: Box<dyn FnOnce(Result<(), String>)>,
) -> Result<(), String> {
    let result = (|| {
        let (paths, reopened) = match startup.selection() {
            shell_contract::ipc::NativeInvocationSelection::StartupLaunch {
                paths,
                reopened_by_installer,
            } => {
                let paths = paths
                    .iter()
                    .map(|path| {
                        path.to_str()
                            .map(str::to_owned)
                            .ok_or_else(|| "a startup path is not valid UTF-8".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (paths, *reopened_by_installer)
            }
            _ => return Err("The selected operation is not a startup launch".into()),
        };
        let pending = shell_contract::runtime::CancelToken::default();
        crate::ipc::native_owner::with_startup_operation_effect(
            &startup,
            &shell_owner,
            &[],
            startup.cancel(),
            &pending,
            || (),
        )
        .ok_or("The captured startup shell authority expired")?;
        let shutdown = startup.shutdown().clone();
        let constraint = shell_contract::ipc::StartupRecoveryConstraint {
            startup: startup.clone(),
            shell: shell_owner.clone(),
            participants: Vec::new(),
            cancel: startup.cancel().clone(),
            pending: pending.clone(),
        };
        let inputs = capture_launch_inputs(
            &services,
            provided.clone(),
            &cx,
            shell_window,
            recovery_issuer,
            constraint,
        )?;
        let manager = Rc::new(inputs.manager.clone());
        let actions = actions::StartupActions::new(
            services.clone(),
            provided,
            cx,
            manager,
            startup.clone(),
            shell_owner.clone(),
            shutdown.clone(),
        )?;
        let launch_actions = actions.launch_actions();
        let deps = Deps::new(
            inputs.manager,
            inputs.gate,
            inputs.store.user_data.clone(),
            paths,
            reopened,
            launch_actions,
            services,
            shutdown,
        );
        let session_writer = session_writer::SessionWriter::new(&inputs.store);
        let mut runtime = Runtime::new(inputs.editors, inputs.store, deps);
        runtime.startup_admission = Some(actions.startup_write_admission()?);
        runtime.session_writer = Some(session_writer);
        runtime.native_paths = Some(NativePaths {
            issuer: startup_issuer,
            owner: shell_owner,
            actions,
            queued: VecDeque::new(),
            active: false,
            settle: None,
            settle_error: None,
        });
        let runtime = Rc::new(RefCell::new(runtime));
        install_live(runtime.clone())?;
        let completed = startup.clone();
        if let Err(error) = Runtime::start(
            runtime,
            Box::new(move |result| {
                completed.finish();
                done(result.map_err(|error| error.0));
            }),
        ) {
            return match release_live() {
                Ok(()) => Err(error.0),
                Err(cleanup) => Err(format!(
                    "{}; recovery runtime cleanup failed: {cleanup}",
                    error.0
                )),
            };
        }
        Ok(())
    })();
    if result.is_err() {
        startup.finish();
    }
    result
}

pub(crate) fn open_native_paths(paths: Vec<String>, reveal: bool) -> Result<(), String> {
    let runtime = LIVE
        .with(|slot| slot.borrow().clone())
        .ok_or("recovery runtime is not installed")?;
    let (issuer, owner, shutdown) = {
        let current = runtime.borrow();
        if current.cancelling || current.shutdown.is_cancelled() || current.phase == Phase::Failed {
            return Err("the native launch runtime is unavailable".into());
        }
        let native = current
            .native_paths
            .as_ref()
            .ok_or("the native launch authority is unavailable")?;
        (
            native.issuer.clone(),
            native.owner.clone(),
            current.shutdown.clone(),
        )
    };
    let operation = issuer::issue_startup(&issuer, &paths, false, &shutdown)?;
    let pending = shell_contract::runtime::CancelToken::default();
    if crate::ipc::native_owner::with_startup_operation_effect(
        &operation,
        &owner,
        &[],
        operation.cancel(),
        &pending,
        || (),
    )
    .is_none()
    {
        operation.finish();
        return Err("the captured native launch owner expired".into());
    }
    let accepted = {
        let mut current = runtime.borrow_mut();
        if current.cancelling || current.shutdown.is_cancelled() || current.phase == Phase::Failed {
            false
        } else if let Some(native) = current.native_paths.as_mut() {
            native.queued.push_back(NativePathRequest {
                operation: operation.clone(),
                paths,
                reveal,
            });
            true
        } else {
            false
        }
    };
    if !accepted {
        operation.finish();
        return Err("the native launch runtime is unavailable".into());
    }
    Runtime::pump_native_paths(runtime);
    Ok(())
}

thread_local! {
    static LIVE: RefCell<Option<Rc<RefCell<Runtime<Editors>>>>> = const { RefCell::new(None) };
    static TIMERS: Cell<(bool, bool)> = const { Cell::new((false, false)) };
    static TIMER_GENERATION: Cell<u64> = const { Cell::new(0) };
    static SHUTDOWN_RUNNING: Cell<bool> = const { Cell::new(false) };
    static ACTIVE_FLUSH: RefCell<Option<Rc<RefCell<FlushState>>>> = const { RefCell::new(None) };
}

pub(crate) fn install_live(runtime: Rc<RefCell<Runtime<Editors>>>) -> Result<(), String> {
    LIVE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err("recovery runtime is already installed".into());
        }
        *slot = Some(runtime);
        Ok(())
    })
}

pub(crate) fn cancel_live_and_drain(done: Box<dyn FnOnce()>) {
    let runtime = LIVE.with(|slot| slot.borrow().clone());
    let Some(runtime) = runtime else {
        return done();
    };
    Runtime::cancel_and_drain(runtime, done);
}

pub(crate) fn release_live() -> Result<(), String> {
    let runtime = LIVE
        .with(|slot| slot.borrow().clone())
        .ok_or_else(|| "recovery runtime is not installed".to_owned())?;
    {
        let runtime = runtime.borrow();
        if matches!(
            runtime.phase,
            Phase::Starting | Phase::Applying | Phase::Awaiting
        ) || !runtime.events.is_empty()
            || runtime.session_in_worker
            || runtime.session_scheduled.is_some()
            || runtime
                .native_paths
                .as_ref()
                .is_some_and(|paths| paths.active || !paths.queued.is_empty())
        {
            return Err("recovery runtime still has pending work".into());
        }
    }
    LIVE.with(|slot| {
        slot.borrow_mut().take();
    });
    TIMER_GENERATION.with(|generation| generation.set(generation.get().wrapping_add(1)));
    TIMERS.with(|timers| timers.set((false, false)));
    Ok(())
}

pub(crate) fn note_live_session() -> Result<(), String> {
    let runtime = LIVE
        .with(|slot| slot.borrow().clone())
        .ok_or_else(|| "recovery runtime is not installed".to_owned())?;
    let (revision, applied) = reserve_session_revision(&runtime)?;
    let latest = runtime.borrow().session_latest.clone();
    Runtime::enqueue(&runtime, move |shell, editors, session, deps| {
        if revision < applied.get() {
            return;
        }
        let (strip, detached) = match deps.manager.tabs.session_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!("[session-record] snapshot failed: {error}");
                return;
            }
        };
        let mut payload = SessionPayload {
            strip: strip.clone(),
            detached: detached.clone(),
            views: Vec::new(),
            copies: std::collections::HashMap::new(),
        };
        let strip = (!strip.is_null()).then(|| jscompat::json_value_from_serde(&strip));
        let detached: Vec<_> = detached
            .iter()
            .map(jscompat::json_value_from_serde)
            .collect();
        let tabs = deps.manager.tabs.clone();
        let views = deps.manager.views.clone();
        let mut copy_for = |tab: &jscompat::JsonValue| {
            let tab = jscompat::json_value_to_serde(tab);
            let id = tab.get("id")?.as_str()?;
            let kind = tab.get("kind")?.as_str()?;
            let path = tab.get("filePath").and_then(serde_json::Value::as_str);
            let contents = tabs.tab_contents(id).map(|view| Contents {
                view,
                views: views.clone(),
            });
            if let Some(contents) = &contents {
                payload.views.push((id.to_owned(), contents.view));
            }
            let copy = shell
                .tab_untitled_copy(editors, id, path, kind, contents.as_ref())
                .map(|copy| {
                    jscompat::json_value_from_serde(
                        &serde_json::json!({ "untitledId":copy.untitled_id,"ext":copy.ext }),
                    )
                });
            payload.copies.insert(id.to_owned(), copy.clone());
            copy
        };
        session.note_session_changed(
            &mut SessionEnv,
            strip.as_ref(),
            &detached,
            Some(&mut copy_for),
        );
        *latest.borrow_mut() = Some((revision, payload));
        applied.set(revision);
        if let Err(error) =
            main_core::shell::recovery_shell::push_hwp_recovery_titles(editors, &deps.manager)
        {
            eprintln!("[recovery] HWP titles could not be updated: {}", error.0);
        }
    });
    schedule_timers(&runtime);
    Ok(())
}

/// The shell window finished closing and no quit is under way, on core. With
/// Quick Look kept in the tray the session is marked closed-to-tray, so a
/// later power loss does not bring back the tabs just closed; on macOS, which
/// keeps running without a window, the empty strip is recorded.
pub(crate) fn note_shell_window_closed() {
    if super::quit::keeps_quick_look_in_tray() {
        finalize_session_lifecycle(
            SessionLifecycle::ClosedToTray,
            Box::new(|result| {
                if let Err(error) = result {
                    eprintln!("shell: marking the session closed to the tray: {error}");
                }
            }),
        );
    } else if cfg!(target_os = "macos")
        && let Err(error) = note_live_session()
    {
        eprintln!("shell: recording live session: {error}");
    }
}

fn reserve_session_revision<E: RecoveryEditors<Contents = Contents> + 'static>(
    runtime: &Rc<RefCell<Runtime<E>>>,
) -> Result<(u64, Rc<Cell<u64>>), String> {
    let mut runtime = runtime
        .try_borrow_mut()
        .map_err(|_| "session runtime is busy")?;
    if runtime.cancelling
        || runtime
            .session_writer
            .as_ref()
            .is_some_and(|writer| writer.is_quiescing())
    {
        return Err("the session writer is quiescing".into());
    }
    runtime.session_next = runtime
        .session_next
        .checked_add(1)
        .ok_or("session revision exhausted")?;
    Ok((runtime.session_next, runtime.session_applied.clone()))
}

#[derive(Clone)]
struct SessionPayload {
    strip: serde_json::Value,
    detached: Vec<serde_json::Value>,
    views: Vec<(String, shell_contract::ViewId)>,
    copies: std::collections::HashMap<String, Option<jscompat::JsonValue>>,
}

impl SessionPayload {
    fn apply(&mut self, updates: &[shell_ipc_w::tabs::SavedSessionUpdate]) {
        for update in updates {
            let view = self
                .views
                .iter()
                .find(|(id, _)| id == &update.id)
                .map(|(_, view)| *view);
            if view != update.view {
                continue;
            }
            let strip = self
                .strip
                .get_mut("tabs")
                .and_then(serde_json::Value::as_array_mut);
            for row in strip.into_iter().flatten().chain(self.detached.iter_mut()) {
                if row.get("id").and_then(serde_json::Value::as_str) != Some(update.id.as_str()) {
                    continue;
                }
                let Some(row) = row.as_object_mut() else {
                    continue;
                };
                row.insert(
                    "title".into(),
                    serde_json::Value::String(update.title.clone()),
                );
                if let Some(path) = &update.path {
                    row.insert("filePath".into(), serde_json::Value::String(path.clone()));
                    self.copies.insert(update.id.clone(), None);
                }
            }
        }
    }
}

/// Only immutable recovery identities cross saved-file continuations.
#[derive(Clone)]
pub(crate) struct SavedSessionSnapshot {
    runtime: Rc<RefCell<Runtime<Editors>>>,
    generation: u64,
    revision: u64,
    baseline: SessionPayload,
}

pub(crate) fn capture_saved_session(
    strip: &serde_json::Value,
    detached: &[serde_json::Value],
    views: &[(String, shell_contract::ViewId)],
    destination: shell_contract::ViewId,
) -> Result<SavedSessionSnapshot, String> {
    capture_session(strip, detached, views, Some(destination))
}

pub(crate) type ActivationSessionSnapshot = SavedSessionSnapshot;

type SessionDone = Box<dyn FnOnce(Result<(), String>)>;
type ActivationSessionRun = Rc<dyn Fn(SessionDone)>;
type PriorSessionWrite = Rc<RefCell<Option<Result<(), String>>>>;
type EventRun<E> = Box<dyn FnOnce(&mut RecoveryShell, &E, &mut SessionRecordProcess, &mut Deps)>;
type SessionWriteDone = Box<dyn FnOnce(Result<(), String>) + Send>;
pub(crate) type LaunchRun = Rc<dyn Fn(LaunchNeed, Box<dyn FnOnce(LaunchReply)>)>;

pub(crate) fn capture_activation_session(
    strip: &serde_json::Value,
    detached: &[serde_json::Value],
    views: &[(String, shell_contract::ViewId)],
) -> Result<ActivationSessionSnapshot, String> {
    capture_session(strip, detached, views, None)
}

fn capture_session(
    strip: &serde_json::Value,
    detached: &[serde_json::Value],
    views: &[(String, shell_contract::ViewId)],
    destination: Option<shell_contract::ViewId>,
) -> Result<SavedSessionSnapshot, String> {
    let runtime = LIVE
        .with(|slot| slot.borrow().clone())
        .ok_or("recovery runtime is not installed")?;
    let (revision, _) = reserve_session_revision(&runtime)?;
    let mut copies = std::collections::HashMap::new();
    if destination.is_some_and(|destination| !views.iter().any(|(_, view)| *view == destination)) {
        return Err("saved document is absent from the session snapshot".into());
    }
    let generation = {
        let current = runtime
            .try_borrow()
            .map_err(|_| "session runtime is busy")?;
        let state = current
            .state
            .as_ref()
            .ok_or("session snapshot cannot capture reentrant editor state")?;
        for tab in strip
            .get("tabs")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .chain(detached.iter())
        {
            let Some(id) = tab.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(kind) = tab.get("kind").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let path = tab.get("filePath").and_then(serde_json::Value::as_str);
            let contents = views
                .iter()
                .find(|(tab_id, _)| tab_id == id)
                .map(|(_, view)| Contents {
                    view: *view,
                    views: state.deps.manager.views.clone(),
                });
            let copy = state
                .shell
                .tab_untitled_copy(state.editors.as_ref(), id, path, kind, contents.as_ref())
                .map(|copy| {
                    jscompat::json_value_from_serde(&serde_json::json!({
                        "untitledId":copy.untitled_id,"ext":copy.ext
                    }))
                });
            copies.insert(id.to_owned(), copy);
        }
        current.generation
    };
    Ok(SavedSessionSnapshot {
        runtime,
        generation,
        revision,
        baseline: SessionPayload {
            strip: strip.clone(),
            detached: detached.to_vec(),
            views: views.to_vec(),
            copies,
        },
    })
}

fn admit_activation_session(
    authority: &shell_ipc_w::tabs::TabActivationAuthority,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
    effects: &shell_contract::hosts::OwnedEffectsHost,
    commit: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    use shell_ipc_w::tabs::TabActivationAuthority as A;
    match authority {
        A::Interactive(origin) => {
            effects.with_owned_effect_many(origin, participants, cancel, pending, commit)
        }
        A::NativeInvocation { document, owner } => {
            crate::ipc::native_owner::with_native_invocation_many_effect(
                document.target(),
                owner,
                participants,
                cancel,
                pending,
                commit,
            )
            .unwrap_or_else(|| Err("the native activation session authority expired".into()))
        }
        A::NativeOperation { operation, owner } => effects.with_native_operation_effect(
            operation,
            owner,
            participants,
            cancel,
            pending,
            commit,
        ),
        A::StartupRecovery {
            startup,
            shell,
            child,
            owner,
        } => effects.with_startup_recovery_effect(
            startup,
            shell,
            child,
            owner,
            participants,
            cancel,
            pending,
            commit,
        ),
        A::NativeCommand {
            operation,
            expected,
            owner,
        } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            effects.with_native_command_preparation_effect(
                operation, expected, &captured, cancel, pending, commit,
            )
        }
        A::NativeCommandTarget {
            document,
            expected,
            owner,
        } => effects.with_native_command_target_effect(
            document.operation(),
            expected,
            document,
            owner,
            participants,
            cancel,
            pending,
            commit,
        ),
        A::StartupCommand {
            startup,
            shell,
            child,
            expected,
            owner,
        } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            effects.with_startup_command_preparation_effect(
                startup, shell, child, expected, &captured, cancel, pending, commit,
            )
        }
        A::StartupCommandTarget {
            startup,
            shell,
            document,
            expected,
            owner,
        } => effects.with_startup_command_target_effect(
            startup,
            shell,
            document.operation(),
            expected,
            document,
            owner,
            participants,
            cancel,
            pending,
            commit,
        ),
        A::CommandRecovery {
            command,
            child,
            owner,
        } => effects.with_command_recovery_target_effect(
            command,
            child,
            owner,
            participants,
            cancel,
            pending,
            commit,
        ),
        A::ExistingDocumentSave {
            path,
            owner,
            additional,
        } => {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(owner.clone());
            captured.extend_from_slice(participants);
            let extra: &[shell_contract::runtime::CancelToken] = additional;
            path.with_effect(effects, &captured, cancel, pending, extra, commit)
        }
        A::SavedDocument {
            document,
            owner,
            additional,
        } => {
            let extra: Vec<_> = additional.iter().collect();
            effects.with_native_saved_document_same_origin(
                document,
                owner,
                cancel,
                participants,
                cancel,
                pending,
                &extra,
                &mut |_| commit(),
            )
        }
        A::NativeSaveReload { receipt, owner } => effects.with_native_save_reload_many_effect(
            receipt,
            owner,
            participants,
            cancel,
            pending,
            commit,
        ),
    }
}

fn activation_session_admission(
    authority: shell_ipc_w::tabs::TabActivationAuthority,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    effects: shell_contract::hosts::OwnedEffectsHost,
) -> SessionAdmission {
    std::sync::Arc::new(move |effect| {
        let mut effect = Some(effect);
        let mut result = None;
        admit_activation_session(
            &authority,
            &participants,
            &cancel,
            &pending,
            &effects,
            &mut || {
                result = Some(effect
                    .take()
                    .ok_or("activation session write admission was repeated")?(
                ));
                Ok(())
            },
        )
        .map_err(|message| main_core::node_fs::NodeError {
            code: None,
            message,
        })?;
        result.ok_or_else(|| main_core::node_fs::NodeError {
            code: None,
            message: "activation session write was not admitted".into(),
        })
    })
}

#[derive(Clone)]
enum ActivationSessionPhase {
    Prepare,
    Write(SessionPlan, SessionAdmission),
    Complete,
}

/// Prepare once from the immutable activation snapshot. Repeated calls resume
/// the retained write plan and never reapply already committed session state.
pub(crate) fn prepare_activation_session(
    captured: ActivationSessionSnapshot,
    authority: shell_ipc_w::tabs::TabActivationAuthority,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    effects: shell_contract::hosts::OwnedEffectsHost,
) -> ActivationSessionRun {
    let phase = Rc::new(RefCell::new(ActivationSessionPhase::Prepare));
    Rc::new(move |done| {
        run_activation_session(
            captured.clone(),
            authority.clone(),
            participants.clone(),
            cancel.clone(),
            pending.clone(),
            effects.clone(),
            phase.clone(),
            done,
        )
    })
}

fn write_activation_session(
    runtime: Rc<RefCell<Runtime<Editors>>>,
    plan: SessionPlan,
    admission: SessionAdmission,
    phase: Rc<RefCell<ActivationSessionPhase>>,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let recorder = plan.recorder_id;
    let revision = plan.revision;
    let finished = runtime.clone();
    write_session_plan(
        runtime,
        plan,
        admission,
        false,
        Box::new(move |result| {
            if result.is_ok() {
                *phase.borrow_mut() = ActivationSessionPhase::Complete;
            } else {
                // write_session_plan retains failed writes for its timer. This
                // continuation now owns this exact plan; prevent a second timer
                // attempt from racing its explicit retry.
                let mut runtime = finished.borrow_mut();
                if runtime
                    .session_scheduled
                    .as_ref()
                    .is_some_and(|(plan, _, _)| {
                        plan.recorder_id == recorder && plan.revision == revision
                    })
                {
                    runtime.session_scheduled.take();
                }
            }
            done(result);
        }),
    );
}

#[allow(clippy::too_many_arguments)]
fn run_activation_session(
    captured: ActivationSessionSnapshot,
    authority: shell_ipc_w::tabs::TabActivationAuthority,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    effects: shell_contract::hosts::OwnedEffectsHost,
    phase: Rc<RefCell<ActivationSessionPhase>>,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let live = LIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|runtime| Rc::ptr_eq(runtime, &captured.runtime))
    });
    if !live || captured.runtime.borrow().generation != captured.generation {
        return done(Err("the captured session runtime was replaced".into()));
    }
    let current = phase.borrow().clone();
    match current {
        ActivationSessionPhase::Complete => return done(Ok(())),
        ActivationSessionPhase::Write(plan, admission) => {
            return write_activation_session(
                captured.runtime.clone(),
                plan,
                admission,
                phase,
                done,
            );
        }
        ActivationSessionPhase::Prepare => {}
    }
    let applied = captured.runtime.borrow().session_applied.clone();
    let latest = captured.runtime.borrow().session_latest.clone();
    let runtime = captured.runtime.clone();
    let revision = captured.revision;
    let baseline = captured.baseline;
    let immediate = !matches!(
        &authority,
        shell_ipc_w::tabs::TabActivationAuthority::Interactive(_)
    );
    let admission = activation_session_admission(
        authority.clone(),
        participants.clone(),
        cancel.clone(),
        pending.clone(),
        effects.clone(),
    );
    let after_runtime = runtime.clone();
    let session_writer = runtime.borrow().session_writer.clone();
    let prior = Rc::new(RefCell::new(None));
    let prior_result = prior.clone();
    flush_scheduled_session(
        runtime.clone(),
        Box::new(move |result| *prior_result.borrow_mut() = Some(result)),
    );
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    Runtime::enqueue_after(
        &runtime,
        move |_, _, session, _| {
            if let Err(error) = prior
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("prior session write was not acknowledged".into()))
            {
                *output.borrow_mut() = Some(Err(error));
                return;
            }
            if session_writer
                .as_ref()
                .is_some_and(|writer| writer.is_quiescing())
            {
                *output.borrow_mut() = Some(Err("the session writer is quiescing".into()));
                return;
            }
            let mut payload = Some(baseline);
            let mut update = None;
            let mut commit = || {
                if revision < applied.get() {
                    return Ok(());
                }
                let payload = payload
                    .take()
                    .ok_or("activation session effect was repeated")?;
                let strip = (!payload.strip.is_null())
                    .then(|| jscompat::json_value_from_serde(&payload.strip));
                let detached: Vec<_> = payload
                    .detached
                    .iter()
                    .map(jscompat::json_value_from_serde)
                    .collect();
                let mut copy_for = |tab: &jscompat::JsonValue| {
                    let tab = jscompat::json_value_to_serde(tab);
                    payload
                        .copies
                        .get(tab.get("id")?.as_str()?)
                        .cloned()
                        .flatten()
                };
                let prepared =
                    session.prepare_session_changed(strip.as_ref(), &detached, Some(&mut copy_for));
                update = Some(match prepared {
                    main_core::shell::session_record::PreparedSessionUpdate::Scheduled(_)
                        if immediate =>
                    {
                        session
                            .session_recorder()
                            .and_then(|recorder| recorder.prepare_fire_write_timer())
                            .map_or(
                                main_core::shell::session_record::PreparedSessionUpdate::Unchanged,
                                main_core::shell::session_record::PreparedSessionUpdate::Immediate,
                            )
                    }
                    other => other,
                });
                *latest.borrow_mut() = Some((revision, payload));
                applied.set(revision);
                Ok(())
            };
            let status = admit_activation_session(
                &authority,
                &participants,
                &cancel,
                &pending,
                &effects,
                &mut commit,
            );
            *output.borrow_mut() = Some(status.map(|()| update));
        },
        move || {
            use main_core::shell::session_record::PreparedSessionUpdate;
            match result
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("activation session was not admitted".into()))
            {
                Err(error) => done(Err(error)),
                Ok(None | Some(PreparedSessionUpdate::Unchanged)) => {
                    *phase.borrow_mut() = ActivationSessionPhase::Complete;
                    done(Ok(()));
                }
                Ok(Some(PreparedSessionUpdate::Scheduled(plan))) => {
                    after_runtime.borrow_mut().session_scheduled = Some((plan, admission, false));
                    *phase.borrow_mut() = ActivationSessionPhase::Complete;
                    done(Ok(()));
                }
                Ok(Some(PreparedSessionUpdate::Immediate(plan))) => {
                    *phase.borrow_mut() =
                        ActivationSessionPhase::Write(plan.clone(), admission.clone());
                    write_activation_session(after_runtime, plan, admission, phase, done);
                }
            }
        },
    );
    schedule_timers(&runtime);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn note_live_session_snapshot_owned(
    captured: SavedSessionSnapshot,
    updates: Vec<shell_ipc_w::tabs::SavedSessionUpdate>,
    origin: shell_contract::ipc::NativeRequestOwner,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let live = LIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|runtime| Rc::ptr_eq(runtime, &captured.runtime))
    });
    if !live || captured.runtime.borrow().generation != captured.generation {
        return done(Err("the captured session runtime was replaced".into()));
    }
    let (revision, applied) = match reserve_session_revision(&captured.runtime) {
        Ok(revision) => revision,
        Err(error) => return done(Err(error)),
    };
    let runtime = captured.runtime.clone();
    let latest = runtime.borrow().session_latest.clone();
    let io_origin = origin.clone();
    let io_participants = participants.clone();
    let io_cancel = cancel.clone();
    let io_pending = pending.clone();
    let io_additional = additional.clone();
    let io_effects = effects.clone();
    let admission: SessionAdmission = std::sync::Arc::new(move |effect| {
        let extra: Vec<_> = io_additional.iter().collect();
        let mut effect = Some(effect);
        let mut result = None;
        io_effects
            .with_owned_effect_many_extra(
                &io_origin,
                &io_participants,
                &io_cancel,
                &io_pending,
                &extra,
                &mut || {
                    result = Some(effect
                        .take()
                        .ok_or("saved session write admission was repeated")?(
                    ));
                    Ok(())
                },
            )
            .map_err(|message| main_core::node_fs::NodeError {
                code: None,
                message,
            })?;
        result.ok_or_else(|| main_core::node_fs::NodeError {
            code: None,
            message: "saved session write was not admitted".into(),
        })
    });
    let after_runtime = runtime.clone();
    let session_writer = runtime.borrow().session_writer.clone();
    let prior = Rc::new(RefCell::new(None));
    let prior_result = prior.clone();
    flush_scheduled_session(
        runtime.clone(),
        Box::new(move |result| *prior_result.borrow_mut() = Some(result)),
    );
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    Runtime::enqueue_after(
        &runtime,
        move |_, _, session, _| {
            if let Err(error) = prior
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("prior session write was not acknowledged".into()))
            {
                *output.borrow_mut() = Some(Err(error));
                return;
            }
            if session_writer
                .as_ref()
                .is_some_and(|writer| writer.is_quiescing())
            {
                *output.borrow_mut() = Some(Err("the session writer is quiescing".into()));
                return;
            }
            let mut baseline = Some(captured.baseline);
            let mut update = None;
            let extra: Vec<_> = additional.iter().collect();
            let status=effects.with_owned_effect_many_extra(&origin,&participants,&cancel,&pending,&extra,&mut || {
            if revision < applied.get() { return Ok(()); }
            let current=latest.borrow().as_ref().filter(|(revision,_)| *revision>=captured.revision)
                .map(|(_,payload)|payload.clone());
            let mut payload=current.or_else(||baseline.take()).ok_or("the saved session was already consumed")?;
            payload.apply(&updates);
            let strip=(!payload.strip.is_null()).then(||jscompat::json_value_from_serde(&payload.strip));
            let detached:Vec<_>=payload.detached.iter().map(jscompat::json_value_from_serde).collect();
            let mut copy_for=|tab:&jscompat::JsonValue| {
                let tab=jscompat::json_value_to_serde(tab);
                payload.copies.get(tab.get("id")?.as_str()?).cloned().flatten()
            };
            let prepared=session.prepare_session_changed(strip.as_ref(),&detached,Some(&mut copy_for));
            update=Some(match prepared {
                main_core::shell::session_record::PreparedSessionUpdate::Scheduled(_) =>
                    session.session_recorder().and_then(|recorder|recorder.prepare_fire_write_timer())
                        .map_or(main_core::shell::session_record::PreparedSessionUpdate::Unchanged,main_core::shell::session_record::PreparedSessionUpdate::Immediate),
                other=>other,
            });
            *latest.borrow_mut()=Some((revision,payload)); applied.set(revision);
            Ok(())
        });
            *output.borrow_mut() = Some(status.map(|()| update));
        },
        move || {
            use main_core::shell::session_record::PreparedSessionUpdate;
            match result
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("saved session was not admitted".into()))
            {
                Err(error) => done(Err(error)),
                Ok(None | Some(PreparedSessionUpdate::Unchanged)) => done(Ok(())),
                Ok(Some(PreparedSessionUpdate::Scheduled(plan))) => {
                    after_runtime.borrow_mut().session_scheduled = Some((plan, admission, false));
                    done(Ok(()));
                }
                Ok(Some(PreparedSessionUpdate::Immediate(plan))) => {
                    write_session_plan(after_runtime, plan, admission, false, done)
                }
            }
        },
    );
    schedule_timers(&runtime);
}

fn recovery_session_admission(
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    receiver: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    effects: shell_contract::hosts::OwnedEffectsHost,
) -> SessionAdmission {
    std::sync::Arc::new(move |effect| {
        let mut effect = Some(effect);
        let mut result = None;
        effects
            .with_recovery_open_effect_many(
                &receipt,
                &receiver,
                &[],
                &cancel,
                &pending,
                &mut || {
                    result = Some(effect
                        .take()
                        .ok_or("recovery session write admission was repeated")?(
                    ));
                    Ok(())
                },
            )
            .map_err(|message| main_core::node_fs::NodeError {
                code: None,
                message,
            })?;
        result.ok_or_else(|| main_core::node_fs::NodeError {
            code: None,
            message: "recovery session write was not admitted".into(),
        })
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn note_live_session_snapshot_recovery(
    captured: SavedSessionSnapshot,
    updates: Vec<shell_ipc_w::tabs::SavedSessionUpdate>,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    receiver: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    effects: shell_contract::hosts::OwnedEffectsHost,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let live = LIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|runtime| Rc::ptr_eq(runtime, &captured.runtime))
    });
    if !live || captured.runtime.borrow().generation != captured.generation {
        return done(Err("the captured session runtime was replaced".into()));
    }
    let (revision, applied) = match reserve_session_revision(&captured.runtime) {
        Ok(value) => value,
        Err(error) => return done(Err(error)),
    };
    let runtime = captured.runtime.clone();
    let latest = runtime.borrow().session_latest.clone();
    let admission = recovery_session_admission(
        receipt.clone(),
        receiver.clone(),
        cancel.clone(),
        pending.clone(),
        effects.clone(),
    );
    let after_runtime = runtime.clone();
    let session_writer = runtime.borrow().session_writer.clone();
    let prior = Rc::new(RefCell::new(None));
    let prior_result = prior.clone();
    flush_scheduled_session(
        runtime.clone(),
        Box::new(move |result| *prior_result.borrow_mut() = Some(result)),
    );
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    Runtime::enqueue_after(
        &runtime,
        move |_, _, session, _| {
            if let Err(error) = prior
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("prior session write was not acknowledged".into()))
            {
                *output.borrow_mut() = Some(Err(error));
                return;
            }
            if session_writer
                .as_ref()
                .is_some_and(|writer| writer.is_quiescing())
            {
                *output.borrow_mut() = Some(Err("the session writer is quiescing".into()));
                return;
            }
            let mut baseline = Some(captured.baseline);
            let mut update = None;
            let status = effects.with_recovery_open_effect_pending(&receipt, &receiver, &cancel, &pending, &mut || {
            if revision < applied.get() { return Ok(()); }
            let current = latest.borrow().as_ref().filter(|(revision, _)| *revision >= captured.revision)
                .map(|(_, payload)| payload.clone());
            let mut payload = current.or_else(|| baseline.take()).ok_or("the recovery session was already consumed")?;
            payload.apply(&updates);
            let strip = (!payload.strip.is_null()).then(|| jscompat::json_value_from_serde(&payload.strip));
            let detached: Vec<_> = payload.detached.iter().map(jscompat::json_value_from_serde).collect();
            let mut copy_for = |tab: &jscompat::JsonValue| {
                let tab = jscompat::json_value_to_serde(tab);
                payload.copies.get(tab.get("id")?.as_str()?).cloned().flatten()
            };
            let prepared=session.prepare_session_changed(strip.as_ref(),&detached,Some(&mut copy_for));
            update=Some(match prepared {
                main_core::shell::session_record::PreparedSessionUpdate::Scheduled(_) =>
                    session.session_recorder().and_then(|recorder|recorder.prepare_fire_write_timer())
                        .map_or(main_core::shell::session_record::PreparedSessionUpdate::Unchanged,main_core::shell::session_record::PreparedSessionUpdate::Immediate),
                other=>other,
            });
            *latest.borrow_mut() = Some((revision, payload));
            applied.set(revision);
            Ok(())
        });
            *output.borrow_mut() = Some(status.map(|()| update));
        },
        move || {
            use main_core::shell::session_record::PreparedSessionUpdate;
            match result
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("the recovery session was not admitted".into()))
            {
                Err(error) => done(Err(error)),
                Ok(None | Some(PreparedSessionUpdate::Unchanged)) => done(Ok(())),
                Ok(Some(PreparedSessionUpdate::Scheduled(plan))) => {
                    after_runtime.borrow_mut().session_scheduled = Some((plan, admission, false));
                    done(Ok(()));
                }
                Ok(Some(PreparedSessionUpdate::Immediate(plan))) => {
                    write_session_plan(after_runtime, plan, admission, false, done)
                }
            }
        },
    );
    schedule_timers(&runtime);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn note_live_session_snapshot_existing_document(
    captured: SavedSessionSnapshot,
    updates: Vec<shell_ipc_w::tabs::SavedSessionUpdate>,
    destination: shell_contract::ipc::NativeSaveDestination,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let gate: SavedSessionGate = std::sync::Arc::new(move |effect| {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_existing_document_destination_effect(
            &destination,
            destination.path(),
            &participants,
            &cancel,
            &pending,
            &extra,
            effect,
        )
    });
    note_live_session_snapshot_guarded(captured, updates, gate, done);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn note_live_session_snapshot_saved_document(
    captured: SavedSessionSnapshot,
    updates: Vec<shell_ipc_w::tabs::SavedSessionUpdate>,
    document: shell_contract::ipc::NativeSavedDocument,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let gate: SavedSessionGate = std::sync::Arc::new(move |effect| {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_native_saved_document_input(
            &document,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut |_| effect(),
        )
    });
    note_live_session_snapshot_guarded(captured, updates, gate, done);
}

type SavedSessionGate = std::sync::Arc<
    dyn Fn(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String> + Send + Sync,
>;

fn note_live_session_snapshot_guarded(
    captured: SavedSessionSnapshot,
    updates: Vec<shell_ipc_w::tabs::SavedSessionUpdate>,
    gate: SavedSessionGate,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let live = LIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|runtime| Rc::ptr_eq(runtime, &captured.runtime))
    });
    if !live || captured.runtime.borrow().generation != captured.generation {
        return done(Err("the captured session runtime was replaced".into()));
    }
    let (revision, applied) = match reserve_session_revision(&captured.runtime) {
        Ok(value) => value,
        Err(error) => return done(Err(error)),
    };
    let runtime = captured.runtime.clone();
    let latest = runtime.borrow().session_latest.clone();
    let io_gate = gate.clone();
    let admission: SessionAdmission = std::sync::Arc::new(move |effect| {
        let mut effect = Some(effect);
        let mut result = None;
        io_gate(&mut || {
            result = Some(effect
                .take()
                .ok_or("saved session write admission was repeated")?(
            ));
            Ok(())
        })
        .map_err(|message| main_core::node_fs::NodeError {
            code: None,
            message,
        })?;
        result.ok_or_else(|| main_core::node_fs::NodeError {
            code: None,
            message: "saved session write was not admitted".into(),
        })
    });
    let after_runtime = runtime.clone();
    let session_writer = runtime.borrow().session_writer.clone();
    let prior = Rc::new(RefCell::new(None));
    let prior_result = prior.clone();
    flush_scheduled_session(
        runtime.clone(),
        Box::new(move |result| *prior_result.borrow_mut() = Some(result)),
    );
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    Runtime::enqueue_after(
        &runtime,
        move |_, _, session, _| {
            if let Err(error) = prior
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("prior session write was not acknowledged".into()))
            {
                *output.borrow_mut() = Some(Err(error));
                return;
            }
            if session_writer
                .as_ref()
                .is_some_and(|writer| writer.is_quiescing())
            {
                *output.borrow_mut() = Some(Err("the session writer is quiescing".into()));
                return;
            }
            let mut baseline = Some(captured.baseline);
            let mut update = None;
            let status = gate(&mut || {
                if revision < applied.get() {
                    return Ok(());
                }
                let current = latest
                    .borrow()
                    .as_ref()
                    .filter(|(revision, _)| *revision >= captured.revision)
                    .map(|(_, payload)| payload.clone());
                let mut payload = current
                    .or_else(|| baseline.take())
                    .ok_or("the saved session was already consumed")?;
                payload.apply(&updates);
                let strip = (!payload.strip.is_null())
                    .then(|| jscompat::json_value_from_serde(&payload.strip));
                let detached: Vec<_> = payload
                    .detached
                    .iter()
                    .map(jscompat::json_value_from_serde)
                    .collect();
                let mut copy_for = |tab: &jscompat::JsonValue| {
                    let tab = jscompat::json_value_to_serde(tab);
                    payload
                        .copies
                        .get(tab.get("id")?.as_str()?)
                        .cloned()
                        .flatten()
                };
                let prepared =
                    session.prepare_session_changed(strip.as_ref(), &detached, Some(&mut copy_for));
                update = Some(match prepared {
                    main_core::shell::session_record::PreparedSessionUpdate::Scheduled(_) => {
                        session
                            .session_recorder()
                            .and_then(|recorder| recorder.prepare_fire_write_timer())
                            .map_or(
                                main_core::shell::session_record::PreparedSessionUpdate::Unchanged,
                                main_core::shell::session_record::PreparedSessionUpdate::Immediate,
                            )
                    }
                    other => other,
                });
                *latest.borrow_mut() = Some((revision, payload));
                applied.set(revision);
                Ok(())
            });
            *output.borrow_mut() = Some(status.map(|()| update));
        },
        move || {
            use main_core::shell::session_record::PreparedSessionUpdate;
            match result
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("saved session was not admitted".into()))
            {
                Err(error) => done(Err(error)),
                Ok(None | Some(PreparedSessionUpdate::Unchanged)) => done(Ok(())),
                Ok(Some(PreparedSessionUpdate::Scheduled(plan))) => {
                    after_runtime.borrow_mut().session_scheduled = Some((plan, admission, false));
                    done(Ok(()));
                }
                Ok(Some(PreparedSessionUpdate::Immediate(plan))) => {
                    write_session_plan(after_runtime, plan, admission, false, done)
                }
            }
        },
    );
    schedule_timers(&runtime);
}

#[cfg(test)]
#[path = "recovery_launch/saved_tests.rs"]
mod saved_tests;

type ShutdownDone = Box<dyn FnOnce(Result<(), Failure>)>;

struct FlushState {
    remaining: usize,
    starting: bool,
    errors: Vec<Option<String>>,
    done: Option<ShutdownDone>,
}

fn finish_flush(state: &Rc<RefCell<FlushState>>) {
    let result = {
        let mut state = state.borrow_mut();
        if state.starting || state.remaining != 0 {
            return;
        }
        let Some(done) = state.done.take() else {
            return;
        };
        let errors: Vec<_> = state.errors.iter().filter_map(Clone::clone).collect();
        let result = if errors.is_empty() {
            Ok(())
        } else {
            Err(Failure(errors.join("; ")))
        };
        (done, result)
    };
    ACTIVE_FLUSH.with(|active| {
        let mut active = active.borrow_mut();
        if active.as_ref().is_some_and(|item| Rc::ptr_eq(item, state)) {
            active.take();
        }
    });
    result.0(result.1);
}

fn flush_editors(
    editors: Vec<
        Rc<
            dyn main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<
                    shell_contract::ViewId,
                    shell_contract::ipc::RecoveryOpenTarget,
                >,
        >,
    >,
    done: ShutdownDone,
    core: Option<std::sync::Arc<dyn shell_contract::runtime::Thread>>,
) {
    let state = Rc::new(RefCell::new(FlushState {
        remaining: editors.len(),
        starting: true,
        errors: vec![None; editors.len()],
        done: Some(done),
    }));
    ACTIVE_FLUSH.with(|active| *active.borrow_mut() = Some(state.clone()));
    if let Some(core) = core {
        let generation = TIMER_GENERATION.with(Cell::get);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(
                main_core::shell::launch_recovery::OS_SESSION_END_WRITE_TIMEOUT_MS as u64,
            ));
            core.post(Box::new(move || timeout_flush(generation)));
        });
    }
    for (index, editor) in editors.into_iter().enumerate() {
        let state = state.clone();
        editor.write_copies_now(Box::new(move |result| {
            {
                let mut state = state.borrow_mut();
                if let Err(Failure(error)) = result {
                    state.errors[index] = Some(error);
                }
                state.remaining -= 1;
            }
            finish_flush(&state);
        }));
    }
    state.borrow_mut().starting = false;
    finish_flush(&state);
}

fn timeout_flush(generation: u64) {
    if TIMER_GENERATION.with(Cell::get) != generation {
        return;
    }
    let Some(state) = ACTIVE_FLUSH.with(|active| active.borrow_mut().take()) else {
        return;
    };
    let outcome = {
        let mut state = state.borrow_mut();
        let Some(done) = state.done.take() else {
            return;
        };
        let mut errors: Vec<_> = state.errors.iter().filter_map(Clone::clone).collect();
        errors.push(format!(
            "recovery shutdown timed out with {} editor writes pending",
            state.remaining
        ));
        (done, Failure(errors.join("; ")))
    };
    outcome.0(Err(outcome.1));
}

pub(crate) fn begin_os_session_end(
    provided: &shell_contract::Provided,
    shared: std::sync::Arc<
        std::sync::Mutex<main_core::packages::electron_utils::recovery_store::SharedRecoveryState>,
    >,
    core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
    done: ShutdownDone,
) -> Result<(), Failure> {
    use main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract;
    if LIVE.with(|slot| slot.borrow().is_none()) {
        return Err(Failure("recovery runtime is not installed".into()));
    }
    let editors =
        provided
            .all::<dyn RecoveryEditorContract<
                    shell_contract::ViewId,
                    shell_contract::ipc::RecoveryOpenTarget,
                >>();
    if editors.is_empty() {
        return Err(Failure("no recovery editors are registered".into()));
    }
    {
        let shared = shared
            .lock()
            .map_err(|_| Failure("recovery state is unavailable".into()))?;
        if SHUTDOWN_RUNNING.with(|running| running.replace(true)) {
            return Err(Failure("recovery shutdown has already started".into()));
        }
        main_core::packages::electron_utils::recovery_store::mark_app_shutting_down(&shared);
    }
    finalize_session_lifecycle(
        SessionLifecycle::OsEnd,
        Box::new(move |session_result| {
            flush_editors(
                editors,
                Box::new(move |result| {
                    let result = match (session_result, result) {
                        (Ok(()), result) => result,
                        (Err(error), Ok(())) => Err(Failure(error)),
                        (Err(error), Err(Failure(other))) => {
                            Err(Failure(format!("{error}; {other}")))
                        }
                    };
                    done(result);
                }),
                Some(core),
            );
        }),
    );
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum SessionLifecycle {
    CleanExit,
    HoldForUpdate,
    UpdateRestart,
    ClosedToTray,
    OsEnd,
}

pub(crate) fn finalize_session_lifecycle(
    event: SessionLifecycle,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let Some(runtime) = LIVE.with(|slot| slot.borrow().clone()) else {
        return done(Err("recovery runtime is not installed".into()));
    };
    if matches!(event, SessionLifecycle::HoldForUpdate) {
        let prepared = Rc::new(RefCell::new(None));
        let output = prepared.clone();
        Runtime::enqueue_after(
            &runtime,
            move |_, _, session, _| {
                *output.borrow_mut() = Some(match session.session_recorder() {
                    Some(recorder) => {
                        recorder.hold_tabs_for_update_restart();
                        Ok(())
                    }
                    None => Err("session recorder is not initialized".to_owned()),
                });
            },
            move || {
                done(
                    prepared
                        .borrow_mut()
                        .take()
                        .unwrap_or_else(|| Err("session hold did not prepare".into())),
                )
            },
        );
        return;
    }
    let terminal = matches!(
        event,
        SessionLifecycle::CleanExit | SessionLifecycle::UpdateRestart | SessionLifecycle::OsEnd
    );
    if terminal && let Some(writer) = &runtime.borrow().session_writer {
        writer.quiesce();
    }
    let flushed = Rc::new(RefCell::new(None));
    let flush_result = flushed.clone();
    flush_scheduled_session(
        runtime.clone(),
        Box::new(move |result| *flush_result.borrow_mut() = Some(result)),
    );
    let prepared = Rc::new(RefCell::new(None));
    let output = prepared.clone();
    let event_runtime = runtime.clone();
    let after_runtime = runtime.clone();
    Runtime::enqueue_after(
        &runtime,
        move |_, _, session, _| {
            match flushed
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err("session flush was not acknowledged".into()))
            {
                Ok(()) => {}
                Err(error) => {
                    *output.borrow_mut() = Some(Err(error));
                    return;
                }
            }
            let Some(recorder) = session.session_recorder() else {
                *output.borrow_mut() = Some(Err("session recorder is not initialized".to_owned()));
                return;
            };
            let plan = match event {
                SessionLifecycle::CleanExit => recorder.prepare_mark_clean_exit(),
                SessionLifecycle::HoldForUpdate => {
                    recorder.hold_tabs_for_update_restart();
                    None
                }
                SessionLifecycle::UpdateRestart => recorder.prepare_mark_update_restart(),
                SessionLifecycle::ClosedToTray => recorder.prepare_mark_closed_to_tray(),
                SessionLifecycle::OsEnd => recorder.prepare_mark_os_session_end(),
            };
            let writer = event_runtime.borrow().session_writer.clone();
            *output.borrow_mut() = Some(match (plan, writer) {
                (_, None) => Err("the native session writer is unavailable".to_owned()),
                (None, Some(writer)) => Ok((None, writer)),
                (Some(plan), Some(writer)) => writer
                    .admission(&plan, terminal)
                    .map(|admission| (Some((plan, admission)), writer)),
            });
        },
        move || match prepared
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Err("session lifecycle did not prepare".into()))
        {
            Err(error) => done(Err(error)),
            Ok((None, writer)) => {
                if terminal {
                    writer.retire();
                }
                done(Ok(()));
            }
            Ok((Some((plan, admission)), writer)) => write_session_plan(
                after_runtime,
                plan,
                admission,
                true,
                Box::new(move |result| {
                    if terminal && result.is_ok() {
                        writer.retire();
                    }
                    done(result);
                }),
            ),
        },
    );
}

fn flush_scheduled_session<E: RecoveryEditors<Contents = Contents> + 'static>(
    runtime: Rc<RefCell<Runtime<E>>>,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let output = Rc::new(RefCell::new(None));
    let prepared = output.clone();
    let event_runtime = runtime.clone();
    let after_runtime = runtime.clone();
    Runtime::enqueue_after(
        &runtime,
        move |_, _, session, _| {
            let Some(recorder) = session.session_recorder() else {
                *prepared.borrow_mut() =
                    Some(Err("session recorder is not initialized".to_owned()));
                return;
            };
            let scheduled = event_runtime.borrow_mut().session_scheduled.take();
            *prepared.borrow_mut() = Some(match scheduled {
                Some((plan, admission, native)) => {
                    let plan = if recorder.write_timer() {
                        recorder.prepare_fire_write_timer()
                    } else {
                        Some(plan)
                    };
                    match plan {
                        None => Ok(None),
                        Some(plan) if native => {
                            let writer = event_runtime.borrow().session_writer.clone();
                            match writer {
                                Some(writer) if writer.is_quiescing() => writer
                                    .admission(&plan, true)
                                    .map(|admission| Some((plan, admission, true))),
                                _ => Ok(Some((plan, admission, true))),
                            }
                        }
                        Some(plan) => Ok(Some((plan, admission, false))),
                    }
                }
                None if recorder.write_timer() => {
                    Err("the pending session write lost its original authority".into())
                }
                None => Ok(None),
            });
        },
        move || match output
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Err("session flush did not prepare".into()))
        {
            Err(error) => done(Err(error)),
            Ok(None) => done(Ok(())),
            Ok(Some((plan, admission, native))) => {
                write_session_plan(after_runtime, plan, admission, native, done)
            }
        },
    );
}

fn write_session_plan<E: RecoveryEditors<Contents = Contents> + 'static>(
    runtime: Rc<RefCell<Runtime<E>>>,
    plan: SessionPlan,
    admission: SessionAdmission,
    native: bool,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let services = {
        let mut current = runtime.borrow_mut();
        if current.session_in_worker {
            Err("a session write is already active".to_owned())
        } else if let Some(state) = current.state.as_ref() {
            let services = state.deps.services.clone();
            current.session_in_worker = true;
            Ok(services)
        } else {
            Err("the session runtime is unavailable".to_owned())
        }
    };
    let services = match services {
        Ok(services) => services,
        Err(error) => return done(Err(error)),
    };
    let finished = runtime.clone();
    let committed = std::sync::Arc::new(plan);
    let worker_plan = committed.clone();
    let retry_admission = admission.clone();
    let completion = shell_ipc_w::home_files::back_to_core(
        services.core.clone(),
        move |result: Result<(), String>| {
            let completion_state = {
                let mut current = finished.borrow_mut();
                if let Some(state) = current.state.as_mut() {
                    if let Some(recorder) = state.session.session_recorder() {
                        recorder.complete_write(&committed, result.clone(), &mut SessionEnv);
                    }
                    if result.is_err() {
                        current.session_scheduled =
                            Some(((*committed).clone(), retry_admission, native));
                    }
                    current.session_in_worker = false;
                    Some((current.cancelling, current.generation))
                } else {
                    None
                }
            };
            let Some((cancelled, generation)) = completion_state else {
                return done(Err(
                    "the session runtime disappeared before write acknowledgement".into(),
                ));
            };
            done(result);
            Runtime::drain(&finished);
            if cancelled {
                let ready = {
                    let current = finished.borrow();
                    !current.in_flight
                        && !current.session_in_worker
                        && current.events.is_empty()
                        && !current.drain_waiters.is_empty()
                        && !current
                            .native_paths
                            .as_ref()
                            .is_some_and(|paths| paths.active)
                };
                if ready {
                    Runtime::advance(
                        finished,
                        generation,
                        LaunchStep::Done(Err(Failure("recovery launch was cancelled".into()))),
                    );
                }
            }
        },
    );
    let mut job = SessionWriteJob(Some(completion));
    services.workers.spawn(Box::new(move || {
        let result = worker_plan
            .write_owned(admission)
            .map_err(|error| error.message);
        if let Some(done) = job.0.take() {
            done(result);
        }
    }));
}

#[derive(Clone, Copy)]
enum Timer {
    Write,
    Startup,
}

fn schedule_timers(runtime: &Rc<RefCell<Runtime<Editors>>>) {
    let timers = {
        let mut runtime = runtime.borrow_mut();
        if runtime.cancelling
            || runtime
                .session_writer
                .as_ref()
                .is_some_and(|writer| writer.is_quiescing())
        {
            return;
        }
        let Some(state) = runtime.state.as_mut() else {
            return;
        };
        let Some(recorder) = state.session.session_recorder() else {
            return;
        };
        (
            recorder.write_timer(),
            recorder.write_delay_ms(),
            recorder.startup_timer(),
            recorder.crash_loop_window_ms(),
            state.editors.services.core.clone(),
        )
    };
    let generation = TIMER_GENERATION.with(Cell::get);
    for (kind, pending, delay) in [
        (Timer::Write, timers.0, timers.1),
        (Timer::Startup, timers.2, timers.3),
    ] {
        if !pending {
            continue;
        }
        let schedule = TIMERS.with(|flags| {
            let (mut write, mut startup) = flags.get();
            let flag = match kind {
                Timer::Write => &mut write,
                Timer::Startup => &mut startup,
            };
            if *flag {
                return false;
            }
            *flag = true;
            flags.set((write, startup));
            true
        });
        if !schedule {
            continue;
        }
        let core = timers.4.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(delay.max(0.0) as u64));
            core.post(Box::new(move || fire_timer(kind, generation)));
        });
    }
}

fn fire_timer(kind: Timer, generation: u64) {
    if TIMER_GENERATION.with(Cell::get) != generation {
        return;
    }
    TIMERS.with(|flags| {
        let (mut write, mut startup) = flags.get();
        match kind {
            Timer::Write => write = false,
            Timer::Startup => startup = false,
        }
        flags.set((write, startup));
    });
    let Some(runtime) = LIVE.with(|slot| slot.borrow().clone()) else {
        return;
    };
    if runtime.borrow().cancelling {
        return;
    }
    let prior = if matches!(kind, Timer::Startup) {
        TIMERS.with(|flags| {
            let (write, _) = flags.get();
            flags.set((write, true));
        });
        let prior = Rc::new(RefCell::new(None));
        let output = prior.clone();
        flush_scheduled_session(
            runtime.clone(),
            Box::new(move |result| {
                TIMERS.with(|flags| {
                    let (write, _) = flags.get();
                    flags.set((write, false));
                });
                *output.borrow_mut() = Some(result);
            }),
        );
        Some(prior)
    } else {
        None
    };
    run_session_timer(runtime, kind, generation, prior);
}

fn run_session_timer(
    runtime: Rc<RefCell<Runtime<Editors>>>,
    kind: Timer,
    generation: u64,
    prior: Option<PriorSessionWrite>,
) {
    if runtime.borrow().cancelling {
        return;
    }
    let prepared = Rc::new(RefCell::new(None));
    let output = prepared.clone();
    let event_runtime = runtime.clone();
    let after_runtime = runtime.clone();
    Runtime::enqueue_after(
        &runtime,
        move |_, _, session, _| {
            if event_runtime
                .borrow()
                .session_writer
                .as_ref()
                .is_some_and(|writer| writer.is_quiescing())
            {
                *output.borrow_mut() = Some(Err("the session writer is quiescing".into()));
                return;
            }
            if TIMER_GENERATION.with(Cell::get) != generation {
                *output.borrow_mut() = Some(Err("the session timer was replaced".into()));
                return;
            }
            if let Some(prior) = prior
                && let Err(error) = prior
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| Err("prior session write was not acknowledged".into()))
            {
                *output.borrow_mut() = Some(Err(error));
                return;
            }
            let Some(recorder) = session.session_recorder() else {
                *output.borrow_mut() = Some(Ok(None));
                return;
            };
            let result = match kind {
                Timer::Write => {
                    let scheduled = event_runtime.borrow_mut().session_scheduled.take();
                    match scheduled {
                        Some((_, admission, native)) => Ok(recorder
                            .prepare_fire_write_timer()
                            .map(|plan| (plan, admission, native))),
                        None if recorder.write_timer() => {
                            Err("the scheduled session write lost its original authority"
                                .to_owned())
                        }
                        None => Ok(None),
                    }
                }
                Timer::Startup => {
                    let writer = event_runtime.borrow().session_writer.clone();
                    match recorder.prepare_fire_startup_timer() {
                        None => Ok(None),
                        Some(plan) => writer
                            .ok_or_else(|| "the native session writer is unavailable".to_owned())
                            .and_then(|writer| {
                                writer
                                    .admission(&plan, false)
                                    .map(|admission| Some((plan, admission, true)))
                            }),
                    }
                }
            };
            *output.borrow_mut() = Some(result);
        },
        move || match prepared
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Err("session timer did not prepare".into()))
        {
            Ok(Some((plan, admission, native))) => write_session_plan(
                after_runtime,
                plan,
                admission,
                native,
                Box::new(|result| {
                    if let Err(error) = result {
                        eprintln!("[session-record] timer write failed: {error}");
                    }
                }),
            ),
            Ok(None) => {}
            Err(error) => eprintln!("[session-record] timer preparation failed: {error}"),
        },
    );
}

fn schedule_live_timers() {
    let runtime = LIVE.with(|slot| slot.borrow().clone());
    if let Some(runtime) = runtime {
        schedule_timers(&runtime);
    }
}

pub(crate) struct LaunchActions {
    pub run: LaunchRun,
}

pub(crate) struct Deps {
    pub manager: Manager,
    pub gate: Rc<LaunchList>,
    pub user_data: String,
    pub launch_paths: Vec<String>,
    pub reopened_by_installer: bool,
    pub actions: LaunchActions,
    pub shutdown: shell_contract::runtime::CancelToken,
    pub services: shell_contract::runtime::Services,
}

impl Deps {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        manager: Manager,
        gate: Rc<LaunchList>,
        user_data: String,
        launch_paths: Vec<String>,
        reopened_by_installer: bool,
        actions: LaunchActions,
        services: shell_contract::runtime::Services,
        shutdown: shell_contract::runtime::CancelToken,
    ) -> Self {
        Self {
            manager,
            gate,
            user_data,
            launch_paths,
            reopened_by_installer,
            actions,
            services,
            shutdown,
        }
    }
}

impl RestoredTabOpenerDeps for Deps {
    fn blocked(&self) -> bool {
        self.gate.blocking()
    }
}

impl LaunchDeps for Deps {
    type Manager = Manager;
    fn gate_blocking(&self) -> bool {
        self.gate.blocking()
    }
    fn gate_publish_none(&mut self) {
        self.gate.publish_none();
    }
    fn manager(&mut self) -> &mut Manager {
        &mut self.manager
    }
}

struct Owned<E: RecoveryEditors<Contents = Contents> + 'static> {
    shell: RecoveryShell,
    editors: Rc<E>,
    store: SharedStore,
    session: SessionRecordProcess,
    deps: Deps,
    pending: Option<LaunchMachine>,
    answer_done: Option<shell_ipc_u::launch_list::ApplyDone>,
}

struct Event<E> {
    run: EventRun<E>,
    after: Option<Box<dyn FnOnce()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Starting,
    Awaiting,
    Applying,
    Done,
    Failed,
}

struct NativePathRequest {
    operation: shell_contract::ipc::NativeInvocationOperation,
    paths: Vec<String>,
    reveal: bool,
}

struct NativePaths {
    issuer: StartupIssuer,
    owner: shell_contract::ipc::NativeRequestOwner,
    actions: Rc<actions::StartupActions>,
    queued: VecDeque<NativePathRequest>,
    active: bool,
    settle: Option<(u64, LaunchMachine)>,
    settle_error: Option<Failure>,
}

type SessionAdmission = main_core::packages::electron_utils::atomic_write::owned::Admission;
type SessionPlan = main_core::shell::session_record::SessionWritePlan;

struct SessionWriteJob(Option<SessionWriteDone>);
impl Drop for SessionWriteJob {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            done(Err("the session write worker was dropped".into()));
        }
    }
}

pub(crate) struct Runtime<E: RecoveryEditors<Contents = Contents> + 'static> {
    state: Option<Owned<E>>,
    phase: Phase,
    generation: u64,
    events: VecDeque<Event<E>>,
    session_in_worker: bool,
    in_flight: bool,
    launch_done: Option<ShutdownDone>,
    shutdown: shell_contract::runtime::CancelToken,
    cancelling: bool,
    drain_waiters: Vec<Box<dyn FnOnce()>>,
    native_paths: Option<NativePaths>,
    session_next: u64,
    session_applied: Rc<Cell<u64>>,
    session_latest: Rc<RefCell<Option<(u64, SessionPayload)>>>,
    session_scheduled: Option<(SessionPlan, SessionAdmission, bool)>,
    session_writer: Option<session_writer::SessionWriter>,
    startup_admission: Option<SessionAdmission>,
}

impl<E: RecoveryEditors<Contents = Contents> + 'static> Runtime<E> {
    pub(crate) fn new(editors: Rc<E>, store: SharedStore, deps: Deps) -> Self {
        let shutdown = deps.shutdown.clone();
        Self {
            state: Some(Owned {
                shell: RecoveryShell::new(),
                editors,
                store,
                session: SessionRecordProcess::new(),
                deps,
                pending: None,
                answer_done: None,
            }),
            phase: Phase::Idle,
            generation: 0,
            events: VecDeque::new(),
            session_in_worker: false,
            in_flight: false,
            launch_done: None,
            shutdown,
            cancelling: false,
            drain_waiters: Vec::new(),
            native_paths: None,
            session_next: 0,
            session_applied: Rc::new(Cell::new(0)),
            session_latest: Rc::new(RefCell::new(None)),
            session_scheduled: None,
            session_writer: None,
            startup_admission: None,
        }
    }

    pub(crate) fn start(
        runtime: Rc<RefCell<Self>>,
        done: Box<dyn FnOnce(Result<(), Failure>)>,
    ) -> Result<(), Failure> {
        let (generation, step) = {
            let mut current = runtime.borrow_mut();
            if current.phase != Phase::Idle || current.cancelling || current.shutdown.is_cancelled()
            {
                return Err(Failure("recovery launch is unavailable".into()));
            }
            let generation = current
                .generation
                .checked_add(1)
                .ok_or_else(|| Failure("recovery generation exhausted".into()))?;
            let state = current
                .state
                .as_ref()
                .ok_or_else(|| Failure("recovery state is unavailable".into()))?;
            let step = LaunchMachine::start(
                state.deps.user_data.clone(),
                state.deps.launch_paths.clone(),
                state.deps.reopened_by_installer,
            );
            current.generation = generation;
            current.launch_done = Some(done);
            current.phase = Phase::Starting;
            (generation, step)
        };
        Self::advance(runtime, generation, step);
        Ok(())
    }

    fn pump_native_paths(runtime: Rc<RefCell<Self>>) {
        enum Next {
            Open(
                Rc<actions::StartupActions>,
                shell_contract::ipc::NativeRequestOwner,
                NativePathRequest,
            ),
            Settle(u64, Box<LaunchMachine>, Result<(), Failure>),
        }
        let next = {
            let mut current = runtime.borrow_mut();
            if current.cancelling || current.shutdown.is_cancelled() {
                return;
            }
            let phase = current.phase;
            let Some(paths) = current.native_paths.as_mut() else {
                return;
            };
            if paths.active || (phase != Phase::Done && paths.settle.is_none()) {
                return;
            }
            if let Some(request) = paths.queued.pop_front() {
                paths.active = true;
                Some(Next::Open(
                    paths.actions.clone(),
                    paths.owner.clone(),
                    request,
                ))
            } else if let Some((generation, machine)) = paths.settle.take() {
                Some(Next::Settle(
                    generation,
                    Box::new(machine),
                    paths.settle_error.take().map_or(Ok(()), Err),
                ))
            } else {
                None
            }
        };
        match next {
            None => {}
            Some(Next::Settle(generation, machine, result)) => {
                let next = runtime.clone();
                flush_scheduled_session(
                    runtime,
                    Box::new(move |flushed| {
                        let result = result.and_then(|()| flushed.map_err(Failure));
                        Self::resume(
                            next,
                            generation,
                            *machine,
                            LaunchReply::Completed(result),
                            None,
                        );
                    }),
                );
            }
            Some(Next::Open(actions, owner, request)) => {
                let completed = request.operation.clone();
                actions.open_paths(
                    request.operation,
                    owner,
                    request.paths,
                    request.reveal,
                    Box::new(move |result| {
                        let next = runtime.clone();
                        flush_scheduled_session(
                            runtime,
                            Box::new(move |flushed| {
                                let runtime = next;
                                let result = result.and_then(|()| flushed.map_err(Failure));
                                completed.finish();
                                let (cancelled, can_finish, generation, settle) = {
                                    let mut current = runtime.borrow_mut();
                                    let cancelled = current.cancelling;
                                    let generation = current.generation;
                                    let settle = if let Some(paths) = current.native_paths.as_mut()
                                    {
                                        paths.active = false;
                                        if let Err(error) = result {
                                            if paths.settle.is_some() {
                                                if paths.settle_error.is_none() {
                                                    paths.settle_error = Some(error);
                                                }
                                            } else {
                                                eprintln!(
                                                    "[recovery] native launch failed: {}",
                                                    error.0
                                                );
                                            }
                                        }
                                        if cancelled { paths.settle.take() } else { None }
                                    } else {
                                        None
                                    };
                                    (
                                        cancelled,
                                        !current.in_flight
                                            && !current.session_in_worker
                                            && current.events.is_empty()
                                            && current.state.is_some(),
                                        generation,
                                        settle,
                                    )
                                };
                                if let Some((generation, machine)) = settle {
                                    Self::resume(
                                        runtime,
                                        generation,
                                        machine,
                                        LaunchReply::Completed(Err(Failure(
                                            "recovery launch was cancelled".into(),
                                        ))),
                                        None,
                                    );
                                } else if cancelled && can_finish {
                                    Self::advance(
                                        runtime,
                                        generation,
                                        LaunchStep::Done(Err(Failure(
                                            "recovery launch was cancelled".into(),
                                        ))),
                                    );
                                } else if !cancelled {
                                    Self::pump_native_paths(runtime);
                                }
                            }),
                        );
                    }),
                );
            }
        }
    }

    fn cancel_and_drain(runtime: Rc<RefCell<Self>>, done: Box<dyn FnOnce()>) {
        let (shutdown, immediate, generation, queued, settle) = {
            let mut current = runtime.borrow_mut();
            current.cancelling = true;
            if let Some(writer) = &current.session_writer {
                writer.quiesce();
            }
            current.drain_waiters.push(done);
            let (active, queued, settle) = match current.native_paths.as_mut() {
                Some(paths) => (
                    paths.active,
                    std::mem::take(&mut paths.queued),
                    if paths.active {
                        None
                    } else {
                        paths.settle.take()
                    },
                ),
                None => (false, VecDeque::new(), None),
            };
            let immediate = !current.in_flight
                && !current.session_in_worker
                && !active
                && current.state.is_some()
                && current.events.is_empty();
            (
                current.shutdown.clone(),
                immediate,
                current.generation,
                queued,
                settle,
            )
        };
        for request in queued {
            request.operation.finish();
        }
        shutdown.cancel();
        if let Some((generation, machine)) = settle {
            Self::resume(
                runtime,
                generation,
                machine,
                LaunchReply::Completed(Err(Failure("recovery launch was cancelled".into()))),
                None,
            );
        } else if immediate {
            Self::advance(
                runtime,
                generation,
                LaunchStep::Done(Err(Failure("recovery launch was cancelled".into()))),
            );
        } else {
            Self::drain(&runtime);
        }
    }

    fn resume(
        runtime: Rc<RefCell<Self>>,
        generation: u64,
        machine: LaunchMachine,
        reply: LaunchReply,
        session: Option<SessionRecordProcess>,
    ) {
        let (mut state, cancelling) = {
            let mut current = runtime.borrow_mut();
            if current.generation != generation
                || !matches!(current.phase, Phase::Starting | Phase::Applying)
            {
                return;
            }
            let Some(state) = current.state.take() else {
                return;
            };
            current.in_flight = false;
            (state, current.cancelling)
        };
        let restored = session.is_some();
        if let Some(session) = session {
            state.session = session;
        }
        let step = if cancelling {
            LaunchStep::Done(Err(Failure("recovery launch was cancelled".into())))
        } else {
            machine.resume(
                reply,
                &mut state.shell,
                state.editors.as_ref(),
                &mut state.deps,
            )
        };
        {
            let mut current = runtime.borrow_mut();
            current.state = Some(state);
            if restored {
                current.session_in_worker = false;
            }
        }
        if restored {
            Self::drain(&runtime);
        }
        Self::advance(runtime, generation, step);
    }

    fn advance(runtime: Rc<RefCell<Self>>, generation: u64, step: LaunchStep) {
        let cancelling = runtime.borrow().cancelling;
        if !matches!(&step, LaunchStep::Done(_)) && cancelling {
            return Self::advance(
                runtime,
                generation,
                LaunchStep::Done(Err(Failure("recovery launch was cancelled".into()))),
            );
        }
        match step {
            LaunchStep::Need(need, machine) => {
                let (services, store, run, session, shutdown) = {
                    let mut current = runtime.borrow_mut();
                    if current.generation != generation {
                        return;
                    }
                    let Some(state) = current.state.as_mut() else {
                        return;
                    };
                    let session = if matches!(&need, LaunchNeed::Prepare { .. }) {
                        Some(std::mem::take(&mut state.session))
                    } else {
                        None
                    };
                    let values = (
                        state.deps.services.clone(),
                        state.store.clone(),
                        state.deps.actions.run.clone(),
                        session,
                        state.deps.shutdown.clone(),
                    );
                    if values.3.is_some() {
                        current.session_in_worker = true;
                    }
                    current.in_flight = true;
                    values
                };
                match need {
                    LaunchNeed::Prepare {
                        user_data,
                        paths,
                        reopened_by_installer,
                    } => {
                        let Some(session) = session else {
                            return;
                        };
                        let admission =
                            runtime.borrow().startup_admission.clone().ok_or_else(|| {
                                "The startup write authority is unavailable".to_owned()
                            });
                        let admission = match admission {
                            Ok(admission) => admission,
                            Err(error) => {
                                return Self::resume(
                                    runtime,
                                    generation,
                                    machine,
                                    LaunchReply::Prepared(Err(Failure(error))),
                                    Some(session),
                                );
                            }
                        };
                        let writer = runtime.borrow().session_writer.clone();
                        let Some(writer) = writer else {
                            return Self::resume(
                                runtime,
                                generation,
                                machine,
                                LaunchReply::Prepared(Err(Failure(
                                    "The session writer is unavailable".into(),
                                ))),
                                Some(session),
                            );
                        };
                        worker::prepare(
                            &services,
                            store,
                            session,
                            user_data,
                            paths,
                            reopened_by_installer,
                            shutdown,
                            admission,
                            writer,
                            move |(session, result)| {
                                Self::resume(
                                    runtime,
                                    generation,
                                    machine,
                                    LaunchReply::Prepared(result),
                                    Some(session),
                                );
                            },
                        );
                    }
                    LaunchNeed::RecordDecisions(decisions) => {
                        worker::decisions(&services, store, decisions, shutdown, move |errors| {
                            Self::resume(
                                runtime,
                                generation,
                                machine,
                                LaunchReply::Decisions(errors),
                                None,
                            );
                        });
                    }
                    LaunchNeed::Settled => {
                        let mut machine = Some(machine);
                        {
                            let mut current = runtime.borrow_mut();
                            if let Some(paths) = current.native_paths.as_mut() {
                                paths.settle = machine.take().map(|machine| (generation, machine));
                            }
                        }
                        if let Some(machine) = machine {
                            let next = runtime.clone();
                            flush_scheduled_session(
                                runtime,
                                Box::new(move |result| {
                                    Self::resume(
                                        next,
                                        generation,
                                        machine,
                                        LaunchReply::Completed(result.map_err(Failure)),
                                        None,
                                    );
                                }),
                            );
                        } else {
                            Self::pump_native_paths(runtime);
                        }
                    }
                    need => run(
                        need,
                        Box::new(move |reply| {
                            Self::resume(runtime, generation, machine, reply, None)
                        }),
                    ),
                }
            }
            LaunchStep::AwaitList(rows, machine) => {
                let gate = {
                    let mut current = runtime.borrow_mut();
                    if current.generation != generation {
                        return;
                    }
                    let Some(state) = current.state.as_mut() else {
                        return;
                    };
                    state.pending = Some(machine);
                    let gate = state.deps.gate.clone();
                    current.phase = Phase::Awaiting;
                    gate
                };
                Self::drain(&runtime);
                schedule_live_timers();
                if runtime.borrow().cancelling {
                    return;
                }
                gate.publish(
                    rows,
                    Rc::new(move |answer, done| {
                        let mut state = {
                            let mut current = runtime.borrow_mut();
                            if current.generation != generation
                                || current.phase != Phase::Awaiting
                                || current.cancelling
                            {
                                return done.settle(Err(shell_contract::IpcError {
                                    name: Some("Error".into()),
                                    message: "recovery answer is no longer pending".into(),
                                }));
                            }
                            let Some(state) = current.state.take() else {
                                return done.settle(Err(shell_contract::IpcError {
                                    name: Some("Error".into()),
                                    message: "recovery state is unavailable".into(),
                                }));
                            };
                            current.phase = Phase::Applying;
                            state
                        };
                        let Some(machine) = state.pending.take() else {
                            runtime.borrow_mut().state = Some(state);
                            return done.settle(Err(shell_contract::IpcError {
                                name: Some("Error".into()),
                                message: "recovery answer is no longer pending".into(),
                            }));
                        };
                        state.answer_done = Some(done);
                        let next = machine.answer(
                            answer,
                            &mut state.shell,
                            state.editors.as_ref(),
                            &mut state.deps,
                        );
                        runtime.borrow_mut().state = Some(state);
                        Self::advance(runtime.clone(), generation, next);
                    }),
                );
            }
            LaunchStep::Done(result) => {
                let result = if runtime.borrow().cancelling {
                    Err(Failure("recovery launch was cancelled".into()))
                } else {
                    result
                };
                let needs_flush = {
                    let current = runtime.borrow();
                    result.is_ok()
                        && current.launch_done.is_some()
                        && (current.session_in_worker
                            || current.session_scheduled.is_some()
                            || !current.events.is_empty())
                };
                if needs_flush {
                    let next = runtime.clone();
                    return flush_scheduled_session(
                        runtime,
                        Box::new(move |flushed| {
                            Self::advance(
                                next,
                                generation,
                                LaunchStep::Done(flushed.map_err(Failure)),
                            );
                        }),
                    );
                }
                let success = result.is_ok();
                let (done, discarded) = {
                    let mut current = runtime.borrow_mut();
                    if current.generation != generation {
                        return;
                    }
                    current.phase = if success { Phase::Done } else { Phase::Failed };
                    let done = current.state.as_mut().and_then(|state| {
                        state.pending.take();
                        state.answer_done.take()
                    });
                    let (_active, discarded) = match current.native_paths.as_mut() {
                        Some(paths) => (
                            paths.active,
                            if success {
                                VecDeque::new()
                            } else {
                                std::mem::take(&mut paths.queued)
                            },
                        ),
                        None => (false, VecDeque::new()),
                    };
                    (done, discarded)
                };
                for request in discarded {
                    request.operation.finish();
                }
                Self::drain(&runtime);
                schedule_live_timers();
                if let Some(done) = done {
                    done.settle(result.clone().map_err(|error| shell_contract::IpcError {
                        name: Some("Error".into()),
                        message: error.0,
                    }));
                }
                let needs_post_flush = {
                    let current = runtime.borrow();
                    success
                        && current.launch_done.is_some()
                        && (current.session_in_worker
                            || current.session_scheduled.is_some()
                            || !current.events.is_empty())
                };
                if needs_post_flush {
                    let next = runtime.clone();
                    return flush_scheduled_session(
                        runtime,
                        Box::new(move |flushed| {
                            Self::advance(
                                next,
                                generation,
                                LaunchStep::Done(flushed.map_err(Failure)),
                            );
                        }),
                    );
                }
                let launch_done = runtime.borrow_mut().launch_done.take();
                if let Some(done) = launch_done {
                    done(result);
                }
                let waiters = {
                    let mut current = runtime.borrow_mut();
                    let active = current
                        .native_paths
                        .as_ref()
                        .is_some_and(|paths| paths.active);
                    if current.cancelling
                        && !active
                        && !current.in_flight
                        && !current.session_in_worker
                        && current.events.is_empty()
                    {
                        std::mem::take(&mut current.drain_waiters)
                    } else {
                        Vec::new()
                    }
                };
                for waiter in waiters {
                    waiter();
                }
                if success {
                    Self::pump_native_paths(runtime);
                }
            }
        }
    }

    pub(crate) fn enqueue(
        runtime: &Rc<RefCell<Self>>,
        event: impl FnOnce(&mut RecoveryShell, &E, &mut SessionRecordProcess, &mut Deps) + 'static,
    ) {
        runtime.borrow_mut().events.push_back(Event {
            run: Box::new(event),
            after: None,
        });
        Self::drain(runtime);
    }

    fn enqueue_after(
        runtime: &Rc<RefCell<Self>>,
        event: impl FnOnce(&mut RecoveryShell, &E, &mut SessionRecordProcess, &mut Deps) + 'static,
        after: impl FnOnce() + 'static,
    ) {
        runtime.borrow_mut().events.push_back(Event {
            run: Box::new(event),
            after: Some(Box::new(after)),
        });
        Self::drain(runtime);
    }

    fn drain(runtime: &Rc<RefCell<Self>>) {
        loop {
            let next = {
                let mut current = runtime.borrow_mut();
                if current.session_in_worker
                    || !matches!(
                        current.phase,
                        Phase::Starting
                            | Phase::Awaiting
                            | Phase::Applying
                            | Phase::Done
                            | Phase::Failed
                    )
                {
                    return;
                }
                let Some(event) = current.events.pop_front() else {
                    drop(current);
                    schedule_live_timers();
                    return;
                };
                let Some(state) = current.state.take() else {
                    current.events.push_front(event);
                    return;
                };
                let phase = current.phase;
                let generation = current.generation;
                current.phase = Phase::Applying;
                (event, state, phase, generation)
            };
            let (event, mut state, phase, generation) = next;
            (event.run)(
                &mut state.shell,
                state.editors.as_ref(),
                &mut state.session,
                &mut state.deps,
            );
            let mut current = runtime.borrow_mut();
            if current.generation != generation || current.phase != Phase::Applying {
                eprintln!("[recovery] runtime generation changed during a session event");
                return;
            }
            current.state = Some(state);
            current.phase = phase;
            drop(current);
            if let Some(after) = event.after {
                after();
            }
            let finish = {
                let current = runtime.borrow();
                current.cancelling
                    && !current.in_flight
                    && !current.session_in_worker
                    && current.events.is_empty()
                    && !current.drain_waiters.is_empty()
                    && !current
                        .native_paths
                        .as_ref()
                        .is_some_and(|paths| paths.active)
            };
            if finish {
                Self::advance(
                    runtime.clone(),
                    generation,
                    LaunchStep::Done(Err(Failure("recovery launch was cancelled".into()))),
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod flush_tests {
    use super::*;
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenFileRequest, RecoveryOpenUntitledRequest,
    };
    use shell_contract::ViewId;

    type Completion = Box<dyn FnOnce(Result<(), Failure>)>;
    struct Editor {
        name: &'static str,
        pending: Rc<RefCell<Option<Completion>>>,
    }
    impl RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget> for Editor {
        fn kind(&self) -> &str {
            self.name
        }
        fn write_copies_now(&self, done: Completion) {
            *self.pending.borrow_mut() = Some(done);
        }
        fn open_file_with_decision(
            &self,
            _: &ViewId,
            _: &RecoveryOpenFileRequest,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            done: Completion,
        ) {
            done(Ok(()));
        }
        fn open_untitled_copy(
            &self,
            _: &ViewId,
            _: &RecoveryOpenUntitledRequest,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            done: Completion,
        ) {
            done(Ok(()));
        }
        fn is_unprotectable(&self, _: &ViewId) -> bool {
            false
        }
    }

    #[test]
    fn waits_for_every_editor_and_reports_destroyed_view() {
        let first = Rc::new(RefCell::new(None::<Completion>));
        let second = Rc::new(RefCell::new(None::<Completion>));
        let answer = Rc::new(RefCell::new(None));
        let saved = answer.clone();
        let editors: Vec<
            Rc<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>,
        > = vec![
            Rc::new(Editor {
                name: "docs",
                pending: first.clone(),
            }),
            Rc::new(Editor {
                name: "sheets",
                pending: second.clone(),
            }),
        ];
        flush_editors(
            editors,
            Box::new(move |result| *saved.borrow_mut() = Some(result)),
            None,
        );
        assert!(answer.borrow().is_none());
        first.borrow_mut().take().unwrap()(Err(Failure("renderer destroyed".into())));
        assert!(answer.borrow().is_none());
        second.borrow_mut().take().unwrap()(Ok(()));
        assert_eq!(
            *answer.borrow(),
            Some(Err(Failure("renderer destroyed".into())))
        );
    }

    #[test]
    fn propagates_all_editor_failures_after_the_last_answer() {
        let first = Rc::new(RefCell::new(None::<Completion>));
        let second = Rc::new(RefCell::new(None::<Completion>));
        let answer = Rc::new(RefCell::new(None));
        let saved = answer.clone();
        flush_editors(
            vec![
                Rc::new(Editor {
                    name: "docs",
                    pending: first.clone(),
                }),
                Rc::new(Editor {
                    name: "pdf",
                    pending: second.clone(),
                }),
            ],
            Box::new(move |result| *saved.borrow_mut() = Some(result)),
            None,
        );
        second.borrow_mut().take().unwrap()(Err(Failure("pdf failed".into())));
        assert!(answer.borrow().is_none());
        first.borrow_mut().take().unwrap()(Err(Failure("docs failed".into())));
        assert_eq!(
            *answer.borrow(),
            Some(Err(Failure("docs failed; pdf failed".into())))
        );
    }

    #[test]
    fn deadline_reports_pending_writes_and_late_answers_do_not_repeat_completion() {
        let pending = Rc::new(RefCell::new(None::<Completion>));
        let answers = Rc::new(RefCell::new(Vec::new()));
        let recorded = answers.clone();
        flush_editors(
            vec![Rc::new(Editor {
                name: "docs",
                pending: pending.clone(),
            })],
            Box::new(move |result| recorded.borrow_mut().push(result)),
            None,
        );
        timeout_flush(TIMER_GENERATION.with(Cell::get));
        assert_eq!(
            *answers.borrow(),
            vec![Err(Failure(
                "recovery shutdown timed out with 1 editor writes pending".into()
            ))]
        );
        pending.borrow_mut().take().unwrap()(Ok(()));
        assert_eq!(answers.borrow().len(), 1);
    }

    #[test]
    fn launch_callback_can_reenter_the_actual_runtime() {
        use shell_contract::ipc::{EventSink, Target};
        use shell_contract::runtime::{AppPaths, PathName, Thread, Views, Workers};
        use shell_contract::{Hosts, IpcValue, WindowId};
        use std::path::PathBuf;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Queue(Mutex<Vec<Box<dyn FnOnce() + Send>>>);
        impl Thread for Queue {
            fn post(&self, job: Box<dyn FnOnce() + Send>) {
                self.0.lock().unwrap().push(job);
            }
            fn is_current(&self) -> bool {
                true
            }
        }
        impl Workers for Queue {
            fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
                self.0.lock().unwrap().push(job);
            }
        }
        struct Paths(PathBuf);
        impl AppPaths for Paths {
            fn path(&self, _: PathName) -> PathBuf {
                self.0.clone()
            }
        }
        struct Events;
        impl EventSink for Events {
            fn emit(&self, _: Target, _: &'static str, _: Vec<IpcValue>) {}
        }
        struct MainWindow;
        impl shell_contract::hosts::MainWindow for MainWindow {
            fn view(&self) -> Option<ViewId> {
                None
            }
            fn window(&self) -> Option<WindowId> {
                None
            }
        }

        let queue = Arc::new(Queue::default());
        let core: Arc<dyn Thread> = queue.clone();
        let views = Arc::new(crate::views::ViewTable::new(
            Arc::new(crate::ipc::outlet::TauriOutlet::default()),
            core.clone(),
        ));
        let mut hosts = Hosts::default();
        shell_ipc_w::tabs::probe_hosts(&mut hosts, &serde_json::Value::Null);
        struct TestDir(std::path::PathBuf);
        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = TestDir(
            std::env::temp_dir().join(format!("session-runtime-{}", jscompat::random_uuid())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let user_data = directory.0.clone();
        let services = shell_contract::runtime::Services {
            ui: core.clone(),
            core,
            workers: queue.clone(),
            events: Arc::new(Events),
            views: views.clone() as Arc<dyn Views>,
            paths: Arc::new(Paths(user_data.clone())),
            hosts: Arc::new(hosts),
        };
        let provided = Rc::new(shell_contract::Provided::default());
        let tabs = shell_ipc_w::tabs::probe_state(&services, provided.clone()).unwrap();
        let gate = Rc::new(LaunchList::new(Arc::new(MainWindow)));
        let store = SharedStore {
            shared: Arc::new(Mutex::new(
                main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new(
                    "test",
                ),
            )),
            user_data: user_data.to_string_lossy().into_owned(),
        };
        let (open, needs_user) = super::super::recovery_openers::live_callbacks(
            services.clone(),
            provided.clone(),
            gate.clone(),
            store.shared.clone(),
            store.user_data.clone(),
        );
        let issuer = Rc::new(shell_contract::ipc::NativeInvocationIssuer::new(
            shell_contract::runtime::CancelToken::default(),
        ));
        let root = issuer::test_startup(&issuer);
        let factory: RecoveryIssuer =
            Rc::new(move |selection, cancel| issuer.issue_recovery_child(&root, selection, cancel));
        let manager = Manager {
            tabs,
            views: views.clone(),
            owner: WindowId(1),
            open,
            needs_user,
            issuer: factory.clone(),
        };
        let editors = Rc::new(Editors {
            services: services.clone(),
            provided,
            issuer: factory,
            owner: WindowId(1),
            startup: None,
        });
        let cancel_manager = Manager {
            tabs: manager.tabs.clone(),
            views: manager.views.clone(),
            owner: manager.owner,
            open: manager.open.clone(),
            needs_user: manager.needs_user.clone(),
            issuer: manager.issuer.clone(),
        };
        let action_manager = Manager {
            tabs: manager.tabs.clone(),
            views: manager.views.clone(),
            owner: manager.owner,
            open: manager.open.clone(),
            needs_user: manager.needs_user.clone(),
            issuer: manager.issuer.clone(),
        };
        let action_editors = editors.clone();
        let action_store = store.clone();
        let action_gate = gate.clone();
        let action_services = services.clone();
        let cancel_editors = editors.clone();
        let cancel_store = store.clone();
        let cancel_gate = gate.clone();
        let cancel_services = services.clone();
        type RuntimeHandle = Rc<RefCell<Option<std::rc::Weak<RefCell<Runtime<Editors>>>>>>;
        let handle: RuntimeHandle = Rc::new(RefCell::new(None));
        let weak_slot = handle.clone();
        let count = Rc::new(Cell::new(0));
        let seen = count.clone();
        let actions = LaunchActions {
            run: Rc::new(move |need, done| {
                if matches!(need, LaunchNeed::RestoreSplitOrHome) {
                    let runtime = weak_slot.borrow().as_ref().unwrap().upgrade().unwrap();
                    let seen = seen.clone();
                    Runtime::enqueue(&runtime, move |_, _, _, _| seen.set(seen.get() + 1));
                    assert!(Runtime::start(runtime, Box::new(|_| {})).is_err());
                }
                done(LaunchReply::Completed(Ok(())));
            }),
        };
        let deps = Deps::new(
            manager,
            gate,
            store.user_data.clone(),
            Vec::new(),
            false,
            actions,
            services,
            shell_contract::runtime::CancelToken::default(),
        );
        let writer = session_writer::SessionWriter::new(&store);
        let mut fixture = Runtime::new(editors, store, deps);
        fixture.session_writer = Some(writer);
        fixture.startup_admission = Some(Arc::new(|effect| Ok(effect())));
        let runtime = Rc::new(RefCell::new(fixture));
        *handle.borrow_mut() = Some(Rc::downgrade(&runtime));
        Runtime::start(runtime.clone(), Box::new(|result| assert!(result.is_ok()))).unwrap();
        for _ in 0..2 {
            let job = queue.0.lock().unwrap().remove(0);
            job();
        }
        assert_eq!(count.get(), 1);
        let plan = {
            let mut state = runtime.borrow_mut();
            state
                .state
                .as_mut()
                .unwrap()
                .session
                .session_recorder()
                .unwrap()
                .prepare_mark_closed_to_tray()
                .unwrap()
        };
        let admission = runtime.borrow().startup_admission.clone().unwrap();
        let committed = Rc::new(Cell::new(false));
        let published = committed.clone();
        write_session_plan(
            runtime.clone(),
            plan,
            admission,
            false,
            Box::new(move |result| {
                result.unwrap();
                published.set(true);
            }),
        );
        let next = Rc::new(Cell::new(false));
        let observed = next.clone();
        Runtime::enqueue(&runtime, move |_, _, _, _| observed.set(true));
        assert!(!committed.get());
        assert!(!next.get());
        let worker = queue.0.lock().unwrap().remove(0);
        worker();
        assert!(!committed.get());
        assert!(!next.get());
        let acknowledgement = queue.0.lock().unwrap().remove(0);
        acknowledgement();
        assert!(committed.get());
        assert!(next.get());
        let actions = LaunchActions {
            run: Rc::new(|_, _| panic!("cancelled preparation must not start an action")),
        };
        let shutdown = shell_contract::runtime::CancelToken::default();
        let deps = Deps::new(
            cancel_manager,
            cancel_gate,
            cancel_store.user_data.clone(),
            Vec::new(),
            false,
            actions,
            cancel_services,
            shutdown.clone(),
        );
        let writer = session_writer::SessionWriter::new(&cancel_store);
        let mut fixture = Runtime::new(cancel_editors, cancel_store, deps);
        fixture.session_writer = Some(writer);
        fixture.startup_admission = Some(Arc::new(|effect| Ok(effect())));
        let runtime = Rc::new(RefCell::new(fixture));
        let answered = Rc::new(Cell::new(false));
        let answer = answered.clone();
        Runtime::start(
            runtime.clone(),
            Box::new(move |result| {
                assert!(result.is_err());
                answer.set(true);
            }),
        )
        .unwrap();
        let drained = Rc::new(Cell::new(false));
        let drained_callback = drained.clone();
        Runtime::cancel_and_drain(runtime, Box::new(move || drained_callback.set(true)));
        assert!(shutdown.is_cancelled());
        assert!(!drained.get());
        assert!(!answered.get());
        let worker = queue.0.lock().unwrap().remove(0);
        worker();
        assert!(!drained.get());
        let acknowledgement = queue.0.lock().unwrap().remove(0);
        acknowledgement();
        assert!(answered.get());
        assert!(drained.get());
        let pending = Rc::new(RefCell::new(None::<Box<dyn FnOnce(LaunchReply)>>));
        let captured = pending.clone();
        let actions = LaunchActions {
            run: Rc::new(move |_, done| {
                *captured.borrow_mut() = Some(done);
            }),
        };
        let deps = Deps::new(
            action_manager,
            action_gate,
            action_store.user_data.clone(),
            Vec::new(),
            false,
            actions,
            action_services,
            shell_contract::runtime::CancelToken::default(),
        );
        let writer = session_writer::SessionWriter::new(&action_store);
        let mut fixture = Runtime::new(action_editors, action_store, deps);
        fixture.session_writer = Some(writer);
        fixture.startup_admission = Some(Arc::new(|effect| Ok(effect())));
        let runtime = Rc::new(RefCell::new(fixture));
        Runtime::start(runtime.clone(), Box::new(|result| assert!(result.is_err()))).unwrap();
        for _ in 0..2 {
            let job = queue.0.lock().unwrap().remove(0);
            job();
        }
        assert!(pending.borrow().is_some());
        let drained = Rc::new(Cell::new(false));
        let flag = drained.clone();
        Runtime::cancel_and_drain(runtime, Box::new(move || flag.set(true)));
        assert!(!drained.get());
        let complete = pending.borrow_mut().take().unwrap();
        complete(LaunchReply::Completed(Err(Failure(
            "cancelled after native cleanup".into(),
        ))));
        assert!(drained.get());
    }
}
