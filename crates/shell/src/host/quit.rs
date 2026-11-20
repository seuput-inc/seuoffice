//! Quit asks every window's unsaved-changes guard, then waits for recovery
//! cleanup, project touch persistence and owned worker shutdown.
use std::sync::{Mutex, OnceLock};

use shell_contract::runtime::Services;
use shell_ipc_w::projects::Projects;
use tauri::{AppHandle, Manager, RunEvent};

mod completion;
#[cfg(any(target_os = "macos", windows))]
mod session_end;
#[cfg(target_os = "macos")]
mod terminate;

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
    /// a quit was asked for: the last window closing ends the process even
    /// where closing it alone would not
    explicit: bool,
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
    explicit: false,
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
    install_os_hooks();
}

/// The quits the menu does not reach: the macOS Dock and log out, Windows
/// shutdown and sign-out. Neither runs in a headless export.
fn install_os_hooks() {
    if crate::headless::is_headless() {
        return;
    }
    let Some(app) = SERVICES
        .get()
        .and_then(|services| services.hosts.get::<super::HostContext>())
        .map(|host| host.app.clone())
    else {
        return;
    };
    #[cfg(target_os = "macos")]
    terminate::install(&app);
    #[cfg(any(target_os = "macos", windows))]
    session_end::install(&app);
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = app;
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
    if code.is_none()
        && state.phase == Phase::Running
        && !last_window_quits(
            state.explicit,
            cfg!(target_os = "macos"),
            crate::headless::is_headless(),
            keeps_quick_look_in_tray(),
        )
    {
        api.prevent_exit();
        return;
    }
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

/// `window-all-closed`: the last window ends the app, except on macOS, in a
/// headless run (which ends itself) and while Quick Look stays in the tray,
/// unless a quit was asked for.
fn last_window_quits(explicit: bool, macos: bool, headless: bool, in_tray: bool) -> bool {
    explicit || !(macos || headless || in_tray)
}

/// `shouldKeepQuickLookInTray()`: Windows with Quick Look's Explorer preview
/// and "keep in tray" both on. False before the Quick Look host exists.
pub(crate) fn keeps_quick_look_in_tray() -> bool {
    SERVICES.get().is_some_and(|services| {
        services
            .hosts
            .get::<shell_ipc_w::quick_look::QuickLookHost>()
            .is_some_and(|runtime| runtime.keeps_in_tray())
    })
}

/// `appQuitting`: a quit was asked for or is already past `Running`.
pub(crate) fn is_quitting() -> bool {
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.explicit || state.phase != Phase::Running
}

/// A person's quit (menu Quit, Cmd+Q, the Dock, the updater's restart):
/// `app.quit()`. Every window closes through its own unsaved-changes guard and
/// the process ends only after the last one is gone and the cleanup below has
/// run. A refused or failed guard cancels the quit ([`quit_vetoed`]). Callable
/// from any thread.
pub(crate) fn request_quit(app: &AppHandle) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    match state.phase {
        Phase::Running => state.explicit = true,
        Phase::RecoveryFailed
        | Phase::SessionFailed
        | Phase::ProjectFailed
        | Phase::WorkerFailed => {
            // the cleanup of an earlier quit failed: asking again retries it
            drop(state);
            app.exit(0);
            return;
        }
        _ => return,
    }
    drop(state);
    let ui = app.clone();
    super::w::post_core_checked(
        move |_, _| {
            mark_editors_shutting_down();
            let closing = ui.clone();
            let close = move || {
                let app = closing.clone();
                if let Err(error) = closing.run_on_main_thread(move || close_every_window(&app)) {
                    quit_not_started(&format!("the UI thread is unreachable: {error}"));
                }
            };
            // `before-quit`: "Restart & Install" holds the tabs as they are now,
            // whatever the closing windows report on the way out
            super::update::with_update_restart_requested(move |restart| {
                if !restart {
                    return close();
                }
                super::recovery_launch::finalize_session_lifecycle(
                    super::recovery_launch::SessionLifecycle::HoldForUpdate,
                    Box::new(move |result| {
                        if let Err(error) = result {
                            eprintln!(
                                "[quit] the tabs were not held for the update restart: {error}"
                            );
                        }
                        close();
                    }),
                )
            });
        },
        || quit_not_started("the core thread refused the quit"),
    );
}

/// On the UI thread. Each window's close goes through its guard; with none left
/// no window event will end the process, so the exit is asked for directly.
fn close_every_window(app: &AppHandle) {
    let windows = app.windows();
    if windows.is_empty() {
        app.exit(0);
        return;
    }
    for window in windows.into_values() {
        if let Err(error) = window.close() {
            eprintln!(
                "[quit] window '{}' did not accept the close: {error}",
                window.label()
            );
        }
    }
}

/// A window's guard refused (or failed) while a quit was under way: the quit is
/// off and the application keeps running. Windows that already closed stay
/// closed, as in Electron.
pub(crate) fn quit_vetoed() {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.phase == Phase::Running {
        state.explicit = false;
    }
}

/// The quit could not ask the editors, so it does not go on: leaving without
/// the guards would lose what they hold.
fn quit_not_started(reason: &str) {
    quit_vetoed();
    reset_editors_shutting_down();
    eprintln!("[quit] {reason}; the application keeps running");
}

/// `before-quit`: no close prompt may fall through to Save from here on and no
/// editor clean-up may delete a recovery copy. On core.
fn mark_editors_shutting_down() {
    if let Some(sheets) = shell_ipc_d1::sheets::views() {
        sheets.mark_shutting_down();
    }
}

/// A vetoed or failed quit puts the latch back down. Posted: callers may run
/// off core.
fn reset_editors_shutting_down() {
    super::w::post_core(|_, _| {
        if let Some(sheets) = shell_ipc_d1::sheets::views() {
            sheets.reset_shutting_down();
        }
    });
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
    // The completion is the one owner of this phase's result: a core that
    // refuses the job, or a continuation that is dropped, fails the phase
    // through its Drop instead of leaving the quit waiting.
    completion::post_with_completion(
        |job| core.post(job),
        move |done| {
            if let Some(state) = services.hosts.get::<shell_ipc_m::state::MStateRef>() {
                state.cancel_prepared_hwp_publications();
            }
            mark_editors_shutting_down();
            if let Err(error) = drain_native_terminals(&services, startup) {
                done.finish(Err(error));
                return;
            }
            let delivery_core = services.core.clone();
            let delivery_services = services.clone();
            super::recovery_openers::cancel_all_and_drain(Box::new(move |recovery| {
                let finish: super::headless_cleanup::Done = Box::new(move |headless| {
                    let result = match (recovery, headless) {
                        (Ok(()), result) | (result, Ok(())) => result,
                        (Err(first), Err(second)) => Err(format!("{first}; {second}")),
                    };
                    delivery_core.post(Box::new(move || {
                        done.finish(
                            result
                                .and_then(|()| drain_native_terminals(&delivery_services, startup)),
                        );
                    }));
                });
                if let Some(cleanup) = cleanup {
                    cleanup.quiesce_and_retry(finish);
                } else {
                    finish(Ok(()));
                }
            }));
        },
        move |result| recovery_drained(startup, result),
    );
}

fn resume_after_failed_quit() {
    if let Some(host) = SERVICES
        .get()
        .and_then(|services| services.hosts.get::<super::HostContext>())
    {
        host.headless_cleanup.resume_after_failed_quit();
    }
    // the application keeps running: its editors prompt again
    reset_editors_shutting_down();
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
            // `--measure-dialog`, the probes, and a quit before the shell window's first
            // frame never started a recovery session: there is nothing to finalize
            if !super::recovery_launch::is_live() {
                return done.finish(Ok(()));
            }
            // "Restart & Install" brings the tabs back after the update; any other
            // quit is a clean exit (`will-quit` of the Electron app)
            super::update::with_update_restart_requested(move |restart| {
                let event = if restart {
                    super::recovery_launch::SessionLifecycle::UpdateRestart
                } else {
                    super::recovery_launch::SessionLifecycle::CleanExit
                };
                super::recovery_launch::finalize_session_lifecycle(
                    event,
                    Box::new(move |result| done.finish(result)),
                )
            })
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
            let done = completion::Completion::new(flush_finished);
            projects.flush_touches(Box::new(move |result| done.finish(result)));
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
        // a core that refuses the result fails this attempt; a later quit retries
        completion::post_with_completion(
            |job| worker_services.core.post(job),
            move |done| done.finish(result),
            move |result| workers_finished(generation, result),
        );
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
    use super::{Phase, Startup, State, last_window_quits};
    use std::collections::HashSet;

    #[test]
    fn stale_worker_completion_cannot_complete_a_retry() {
        let mut state = State {
            startup: Startup::Ready,
            phase: Phase::Workers,
            app: None,
            worker_generation: 2,
            exit_code: 0,
            explicit: false,
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
            explicit: false,
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

    #[test]
    fn the_last_window_quits_except_on_macos_and_headless_until_a_quit_is_asked() {
        assert!(last_window_quits(false, false, false, false));
        assert!(!last_window_quits(false, true, false, false));
        assert!(!last_window_quits(false, false, true, false));
        assert!(last_window_quits(true, true, false, false));
        assert!(last_window_quits(true, false, true, false));
    }

    #[test]
    fn a_quick_look_tray_keeps_the_app_after_the_last_window_until_a_quit_is_asked() {
        assert!(!last_window_quits(false, false, false, true));
        assert!(last_window_quits(true, false, false, true));
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
