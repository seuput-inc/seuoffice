//! Normal quit waits for project touch persistence and owned worker shutdown.
use std::sync::{Mutex, OnceLock};

use shell_contract::runtime::Services;
use shell_ipc_w::projects::Projects;
use tauri::{AppHandle, RunEvent};

mod completion;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Startup {
    Pending,
    Ready,
    Failed,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Waiting,
    Recovery,
    RecoveryFailed,
    Session,
    SessionFailed,
    Flushing,
    ProjectFailed,
    Workers,
    WorkerFailed,
    Updater,
    Marking,
    Ready,
    ExitArmed,
    Finished,
}
struct State {
    startup: Startup,
    phase: Phase,
    app: Option<AppHandle>,
    worker_generation: u64,
    exit_code: i32,
}
impl State {
    fn accepts_worker(&self, generation: u64) -> bool {
        self.phase == Phase::Workers && self.worker_generation == generation
    }
    fn observe_exit_code(&mut self, code: i32) {
        if self.exit_code == 0 && code != 0 {
            self.exit_code = code;
        }
    }
}
static STATE: Mutex<State> = Mutex::new(State {
    startup: Startup::Pending,
    phase: Phase::Running,
    app: None,
    worker_generation: 0,
    exit_code: 0,
});
static SERVICES: OnceLock<Services> = OnceLock::new();

/// Install immediately after constructing Services, before starting the shell.
pub(crate) fn register_services(services: Services) -> Result<(), String> {
    SERVICES
        .set(services)
        .map_err(|_| "quit services already registered".to_owned())?;
    start_if_possible();
    Ok(())
}

/// Called on core after registration, MCP start and first-window creation succeed.
pub(crate) fn startup_ready() {
    startup_finished(Startup::Ready);
}
/// Called on every terminal startup failure, including failure before Provided.
pub(crate) fn startup_failed() {
    startup_finished(Startup::Failed);
}
fn startup_finished(result: Startup) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.startup != Startup::Pending {
        return;
    }
    if result == Startup::Failed {
        state.observe_exit_code(if crate::headless::is_headless() { 3 } else { 1 });
    }
    state.startup = result;
    drop(state);
    start_if_possible();
}

pub(crate) fn on_run_event(app: &AppHandle, event: &RunEvent) {
    let RunEvent::ExitRequested { api, code, .. } = event else {
        return;
    };
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let requested = code.unwrap_or(0);
    state.observe_exit_code(requested);
    if matches!(
        state.phase,
        Phase::Ready | Phase::ExitArmed | Phase::Finished
    ) {
        if requested == state.exit_code || state.phase == Phase::Finished {
            state.phase = Phase::Finished;
            return;
        }
        api.prevent_exit();
        let code = state.exit_code;
        state.phase = Phase::ExitArmed;
        drop(state);
        app.exit(code);
        return;
    }
    api.prevent_exit();
    match state.phase {
        Phase::Running => {
            state.app = Some(app.clone());
            state.phase = Phase::Waiting;
        }
        Phase::WorkerFailed => {
            state.phase = Phase::Workers;
            drop(state);
            start_workers();
            return;
        }
        Phase::RecoveryFailed | Phase::SessionFailed | Phase::ProjectFailed => {
            state.phase = Phase::Waiting;
        }
        _ => return,
    }
    drop(state);
    start_if_possible();
}

fn drain_native_terminals(services: &Services, startup: Startup) -> Result<(), String> {
    if !services.core.is_current() {
        return Err("Native terminal drain requires the original core thread".into());
    }
    super::w_tabs::retry_retained_replies();
    if let Some(state) = services.hosts.get::<shell_ipc_m::state::MStateRef>() {
        state.wake_hwp_publications(services.core.clone());
    }
    let host = services
        .hosts
        .get::<shell_contract::runtime::NativeViewRetirementsHost>();
    let Some(host) = host else {
        return if startup == Startup::Failed {
            Ok(())
        } else {
            Err("Native terminal delivery services are unavailable at quit".into())
        };
    };
    host.drain_native_terminals()?;
    if host.pending_native_terminals()? != 0 {
        return Err("Native terminal listener deliveries remain outstanding".into());
    }
    Ok(())
}

fn start_if_possible() {
    let Some(services) = SERVICES.get().cloned() else {
        return;
    };
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.phase != Phase::Waiting || state.startup == Startup::Pending {
        return;
    }
    let startup = state.startup;
    state.phase = Phase::Recovery;
    drop(state);
    let cleanup = services
        .hosts
        .get::<super::HostContext>()
        .map(|host| host.headless_cleanup.clone());
    let core = services.core.clone();
    let dispatch = services.core.clone();
    dispatch.post(Box::new(move || {
        if let Some(state) = services.hosts.get::<shell_ipc_m::state::MStateRef>() {
            state.cancel_prepared_hwp_publications();
        }
        if let Err(error) = drain_native_terminals(&services, startup) {
            recovery_drained(startup, Err(error));
            return;
        }
        let delivery_services = services.clone();
        super::recovery_openers::cancel_all_and_drain(Box::new(move |recovery| {
            let finish = Box::new(move |headless| {
                let result = match (recovery, headless) {
                    (Ok(()), result) | (result, Ok(())) => result,
                    (Err(first), Err(second)) => Err(format!("{first}; {second}")),
                };
                core.post(Box::new(move || {
                    let result =
                        result.and_then(|()| drain_native_terminals(&delivery_services, startup));
                    recovery_drained(startup, result);
                }));
            });
            if let Some(cleanup) = cleanup {
                cleanup.quiesce_and_retry(finish);
            } else {
                finish(Ok(()));
            }
        }));
    }));
}

fn resume_after_failed_quit() {
    if let Some(host) = SERVICES
        .get()
        .and_then(|services| services.hosts.get::<super::HostContext>())
    {
        host.headless_cleanup.resume_after_failed_quit();
    }
}

fn recovery_drained(startup: Startup, result: Result<(), String>) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.phase != Phase::Recovery {
        return;
    }
    if let Err(error) = result {
        state.phase = Phase::RecoveryFailed;
        drop(state);
        resume_after_failed_quit();
        eprintln!("[quit] native recovery cleanup failed; a later quit retries: {error}");
        return;
    }
    state.phase = Phase::Session;
    drop(state);
    start_session_flush(startup);
}

fn start_session_flush(startup: Startup) {
    if startup == Startup::Failed || crate::headless::is_headless() {
        session_flushed(startup, Ok(()));
        return;
    }
    let Some(services) = SERVICES.get().cloned() else {
        session_flushed(startup, Err("session services unavailable at quit".into()));
        return;
    };
    completion::post_with_completion(
        |job| services.core.post(job),
        |done| {
            super::recovery_launch::finalize_session_lifecycle(
                super::recovery_launch::SessionLifecycle::CleanExit,
                Box::new(move |result| done.finish(result)),
            )
        },
        move |result| session_flushed(startup, result),
    );
}

fn session_flushed(startup: Startup, result: Result<(), String>) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.phase != Phase::Session {
        return;
    }
    if let Err(error) = result {
        state.phase = Phase::SessionFailed;
        drop(state);
        resume_after_failed_quit();
        eprintln!("[quit] final session flush failed; a later quit retries: {error}");
        return;
    }
    state.phase = Phase::Flushing;
    drop(state);
    let accepted = super::w::on_core_local(|provided| {
        if let Some(projects) = provided.all::<Projects>().into_iter().next() {
            projects.flush_touches(Box::new(flush_finished));
        } else {
            flush_finished(Ok(()));
        }
    });
    if !accepted {
        // Failed registration may never have installed Provided or Projects.
        // This does not bypass a live core's cleanup from another thread.
        if startup == Startup::Failed
            && SERVICES
                .get()
                .is_some_and(|services| services.core.is_current())
        {
            flush_finished(Ok(()));
        } else {
            flush_finished(Err("project services unavailable at quit".into()));
        }
    }
}

fn flush_finished(result: Result<(), String>) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.phase != Phase::Flushing {
        return;
    }
    if let Err(error) = result {
        state.phase = Phase::ProjectFailed;
        drop(state);
        resume_after_failed_quit();
        eprintln!("[quit] project cleanup failed; a later quit retries: {error}");
        return;
    }
    let startup = state.startup;
    drop(state);
    let Some(services) = SERVICES.get().cloned() else {
        return flush_finished(Err(
            "Quit services are unavailable before worker shutdown".into()
        ));
    };
    if let Err(error) = drain_native_terminals(&services, startup).and_then(|()| {
        if super::w_tabs::indeterminate_retained_replies() != 0 {
            Err("Activation publication has an indeterminate completion at quit".into())
        } else if super::w_tabs::pending_retained_replies() != 0 {
            Err("Activation publication replies remain outstanding at quit".into())
        } else {
            services
                .hosts
                .get::<super::HostContext>()
                .map_or(Ok(()), |host| host.headless_cleanup.ensure_drained())
        }
    }) {
        return flush_finished(Err(error));
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.phase != Phase::Flushing {
        return;
    }
    state.phase = Phase::Workers;
    drop(state);
    start_workers();
}

fn start_workers() {
    let Some(services) = SERVICES.get().cloned() else {
        return;
    };
    let generation = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        if state.phase != Phase::Workers {
            return;
        }
        let Some(next) = state.worker_generation.checked_add(1) else {
            state.phase = Phase::WorkerFailed;
            eprintln!("[quit] worker generation exhausted");
            return;
        };
        state.worker_generation = next;
        next
    };
    let worker_services = services.clone();
    services.workers.spawn(Box::new(move || {
        let result = worker_services
            .hosts
            .get::<std::sync::Arc<super::app_mcp::AppMcp>>()
            .map_or(Ok(()), |mcp| mcp.stop())
            .and_then(|()| super::w_home::shutdown(&worker_services.hosts));
        worker_services
            .core
            .post(Box::new(move || workers_finished(generation, result)));
    }));
}

fn workers_finished(generation: u64, result: Result<(), String>) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if !state.accepts_worker(generation) {
        return;
    }
    if let Err(error) = result {
        state.phase = Phase::WorkerFailed;
        drop(state);
        eprintln!("[quit] worker shutdown failed; a later quit retries: {error}");
        return;
    }
    state.phase = Phase::Updater;
    drop(state);
    super::update::will_quit(updater_finished);
}

fn updater_finished() {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.phase != Phase::Updater {
        return;
    }
    state.phase = Phase::Marking;
    drop(state);
    super::crash::mark_ended_on_core();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.phase != Phase::Marking {
        return;
    }
    state.phase = Phase::Ready;
    let app = state.app.clone();
    drop(state);
    if let Some(app) = app {
        let exit_app = app.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            if state.phase != Phase::Ready {
                return;
            }
            state.phase = Phase::ExitArmed;
            let code = state.exit_code;
            drop(state);
            exit_app.exit(code);
        }) {
            eprintln!("[quit] cannot schedule exit: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Phase, Startup, State};
    use std::collections::HashSet;

    #[test]
    fn stale_worker_completion_cannot_complete_a_retry() {
        let mut state = State {
            startup: Startup::Ready,
            phase: Phase::Workers,
            app: None,
            worker_generation: 2,
            exit_code: 0,
        };
        assert!(!state.accepts_worker(1));
        assert!(state.accepts_worker(2));
        state.phase = Phase::Updater;
        assert!(!state.accepts_worker(2));
    }

    #[test]
    fn failure_code_survives_normal_duplicate_and_cleanup() {
        let mut state = State {
            startup: Startup::Pending,
            phase: Phase::Waiting,
            app: None,
            worker_generation: 0,
            exit_code: 0,
        };
        state.observe_exit_code(1);
        state.phase = Phase::Flushing;
        state.observe_exit_code(0);
        state.phase = Phase::Workers;
        state.observe_exit_code(0);
        state.phase = Phase::Ready;
        assert_eq!(state.exit_code, 1);
        state.observe_exit_code(2);
        assert_eq!(state.exit_code, 1);
    }

    // Frozen policy and the event-to-code mapping are in quit/Properties.md.
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    struct Model {
        phase: u8,
        startup: u8,
        startup_steps: u8,
        drains: u8,
        sessions: u8,
        recovery_failures: u8,
        session_failures: u8,
        project_failures: u8,
        native_delivered: bool,
        actor_ack: bool,
        flushes: u8,
        stops: u8,
        updater: u8,
        exited: bool,
    }
    impl Model {
        fn actions(self) -> Vec<Self> {
            let mut out = Vec::new();
            let mut next = self;
            if self.startup == 0 {
                if self.startup_steps < 3 {
                    next.startup_steps += 1;
                    out.push(next);
                    next = self;
                } else {
                    next.startup = 1;
                    out.push(next);
                    next = self;
                }
                next.startup = 2;
                out.push(next);
            }
            next = self;
            match self.phase {
                0 => {
                    next.phase = 1;
                    out.push(next);
                }
                1 if self.startup != 0 => {
                    next.phase = 8;
                    out.push(next);
                }
                8 => {
                    if !self.native_delivered {
                        next.native_delivered = true;
                        out.push(next);
                    } else if !self.actor_ack {
                        next.actor_ack = true;
                        out.push(next);
                    } else {
                        next.drains = 1;
                        if self.startup == 1 {
                            next.phase = 10;
                        } else {
                            next.phase = 2;
                            next.flushes = 1;
                        }
                        out.push(next);
                    }
                    if self.recovery_failures == 0 {
                        next = self;
                        next.phase = 9;
                        next.recovery_failures = 1;
                        out.push(next);
                    }
                }
                9 => {
                    next.phase = 1;
                    out.push(next);
                } // a new quit retries retained cleanup
                10 => {
                    next.sessions = 1;
                    next.phase = 2;
                    next.flushes = 1;
                    out.push(next);
                    if self.session_failures == 0 {
                        next = self;
                        next.phase = 11;
                        next.session_failures = 1;
                        out.push(next);
                    }
                }
                11 => {
                    next.phase = 1;
                    out.push(next);
                } // session failure re-drains first
                // A settings-write error is caught by Projects; an actual
                // cleanup failure blocks shutdown and an explicit quit retries.
                2 => {
                    next.phase = 3;
                    next.stops += 1;
                    out.push(next);
                    if self.project_failures == 0 {
                        next = self;
                        next.phase = 12;
                        next.project_failures = 1;
                        out.push(next);
                    }
                }
                12 => {
                    next.phase = 1;
                    out.push(next);
                }
                3 => {
                    next.phase = 4;
                    out.push(next); // worker failure
                    next.phase = 5;
                    next.updater += 1;
                    out.push(next);
                }
                4 if self.stops < 2 => {
                    next.phase = 3;
                    next.stops += 1;
                    out.push(next);
                }
                5 => {
                    next.phase = 6;
                    out.push(next);
                } // updater and marker done
                6 => {
                    next.phase = 7;
                    next.exited = true;
                    out.push(next);
                }
                _ => {}
            }
            // Repeated quit and duplicate callbacks are no-ops while stopping.
            if (1..=5).contains(&self.phase) || matches!(self.phase, 8 | 10) {
                out.push(self);
            }
            out
        }
    }

    #[test]
    fn bounded_quit_interleavings_keep_cleanup_before_exit() {
        let initial = Model {
            phase: 0,
            startup: 0,
            startup_steps: 0,
            drains: 0,
            sessions: 0,
            recovery_failures: 0,
            session_failures: 0,
            project_failures: 0,
            native_delivered: false,
            actor_ack: false,
            flushes: 0,
            stops: 0,
            updater: 0,
            exited: false,
        };
        let mut seen = HashSet::new();
        let mut pending = vec![initial];
        while let Some(state) = pending.pop() {
            if !seen.insert(state) {
                continue;
            }
            assert!(
                state.drains <= 1
                    && state.sessions <= 1
                    && state.flushes <= 1
                    && state.updater <= 1
            );
            if state.stops > 0 || state.flushes > 0 {
                assert_eq!(state.drains, 1);
                assert_eq!(state.sessions, u8::from(state.startup == 1));
                assert!(state.native_delivered && state.actor_ack);
            }
            if state.phase >= 2 && state.startup == 1 {
                assert_eq!(state.startup_steps, 3);
            }
            if state.exited {
                assert!(
                    state.startup != 0
                        && state.drains == 1
                        && state.sessions == u8::from(state.startup == 1)
                        && state.flushes == 1
                        && state.native_delivered
                        && state.actor_ack
                        && state.stops > 0
                        && state.updater == 1
                );
            }
            pending.extend(state.actions());
        }
        assert!(seen.iter().any(|state| state.exited));
        assert!(
            seen.iter()
                .any(|state| state.phase == 1 && state.startup_steps < 3)
        );
        assert!(seen.iter().any(|state| state.exited && state.startup == 2));
        assert!(seen.iter().any(|state| state.phase == 4));
        assert!(seen.iter().any(|state| state.exited && state.stops == 2));
        assert!(
            seen.iter()
                .any(|state| state.phase == 9 && state.stops == 0)
        );
        assert!(
            seen.iter()
                .any(|state| state.phase == 11 && state.stops == 0)
        );
        assert!(
            seen.iter()
                .any(|state| state.phase == 12 && state.stops == 0)
        );
    }
}
