//! `SeuOffice --e2e <scenario.json> --out <dir>`: the real-app test runner of gate G3.
//!
//! Gated the way `--probe` is: a runtime flag, compiled into every build, inert without it. The
//! shell starts as usual, fully (with `SEUOFFICE_SHELL_PARTIAL` it refuses), with its normal
//! windows. Once the first shell window is up the runner takes over a thread of its own and
//! executes the scenario's steps in order on the real app: it opens files the way an OS
//! open-file request does, types through native key events, answers native dialogs through a
//! hook of the dialog host, evaluates read-only expressions in the active editor page and
//! writes `<out>/e2e.jsonl` (one JSON object per step) plus its artifacts. It never decides
//! pass or fail: a checker reads the lines.
//!
//! The runner drives the app like a user and opens nothing to the outside: no port, socket or
//! pipe, the scenario file is its only input. It refuses to start (exit 2, message on stderr)
//! unless `SEUOFFICE_USER_DATA` names a folder holding the marker file
//! [`PROFILE_MARKER`], which the CI workflow creates in its throwaway profile, so a person's
//! installed app, profile or keychain can never be the target.
//!
//! Exit codes: 0 when every step ran, 2 when the run was refused (profile, flags, scenario),
//! 3 when a step could not run (the runner stops there; the core thread not answering for 90 s
//! is such a step), 124 past the scenario's deadline, which counts from the process start. A
//! stalled run leaves what the process was doing in the run log's trace and, on macOS, in
//! `<out>/hang-<core-stalled|deadline>.txt` (the system `sample` of every thread).
//!
//! | op | how |
//! |---|---|
//! | `open` | `host::os::open_paths`, the path of macOS open-file events and the single-instance argv |
//! | `newDocument` | `ShellTabs::new_document`, as the Home tiles and the New menu do |
//! | `waitReady` | the tab of the file `open` named (after `newDocument`: a tab other than the one before) is active and its document reached the bridge's render-ready state, then the page is quiet |
//! | `type`, `shortcut`, `ime` | native key events (`input_mac.rs`, `input_win.rs`) |
//! | `clipboard` | the OS clipboard on the UI thread |
//! | `query` | `PageOps::evaluate` on the active editor's webview; no new IPC channel exists |
//! | `dialog` | answers the next native dialog (`dialog.rs`) |
//! | `exportPdf` | the active menu's Export as PDF item, with its save dialog answered |
//! | `screenshot` | the app window through `xcap`, else the page's own capture |

pub(crate) mod dialog;
#[cfg(target_os = "macos")]
mod input_mac;
#[cfg(windows)]
mod input_win;
mod report;
mod scenario;
mod steps;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::host::HostContext;
use crate::host::w::on_core_within;
use crate::trace::trace;

/// The file the workflow puts in its throwaway profile folder
pub const PROFILE_MARKER: &str = ".seuoffice-e2e-profile";

const EXIT_REFUSED: i32 = 2;
const EXIT_UNRUNNABLE: i32 = 3;
const EXIT_DEADLINE: i32 = 124;
/// how long the runner leaves the event loop to end the process before it ends it itself
const EXIT_GRACE: Duration = Duration::from_secs(30);
/// `sample` takes three seconds of samples; a longer wait means it is stuck itself
#[cfg(target_os = "macos")]
const SAMPLE_WAIT: Duration = Duration::from_secs(30);
/// the core watch calls the core thread late after this
const CORE_LATE: Duration = Duration::from_secs(10);

const USER_DATA_ENV: &str = "SEUOFFICE_USER_DATA";
const PARTIAL_ENV: &str = "SEUOFFICE_SHELL_PARTIAL";

pub struct Request {
    pub scenario: PathBuf,
    pub out: PathBuf,
}

struct Loaded {
    request: Request,
    scenario: scenario::Scenario,
}

static LOADED: OnceLock<Option<Loaded>> = OnceLock::new();

/// The `--e2e` request of this process, if any. Valid once [`init`] ran.
pub fn request() -> Option<&'static Request> {
    LOADED
        .get()
        .and_then(Option::as_ref)
        .map(|loaded| &loaded.request)
}

/// Records what goes wrong inside a page, in every frame, for the runner to read back: a hang
/// or a blank editor says nothing about why, and the page's own console is out of reach.
const PAGE_ERROR_SCRIPT: &str = "(() => { \
    const log = (kind, text) => { try { (window.__e2eErrors = window.__e2eErrors || []).push(kind + ': ' + text) } catch (e) {} }; \
    addEventListener('error', (e) => log('error', e.message + ' at ' + e.filename + ':' + e.lineno)); \
    addEventListener('unhandledrejection', (e) => log('rejection', String(e.reason && e.reason.stack ? e.reason.stack : e.reason))); \
    addEventListener('securitypolicyviolation', (e) => log('csp', e.violatedDirective + ' blocked ' + e.blockedURI)); \
    const original = console.error; \
    console.error = function () { log('console.error', Array.prototype.map.call(arguments, String).join(' ')); return original.apply(console, arguments) }; \
})()";

/// The recorder script for a view's builder; only a `--e2e` run has one
pub(crate) fn page_error_script() -> Option<&'static str> {
    request().map(|_| PAGE_ERROR_SCRIPT)
}

/// The first thing `main` does after the worker modes: reads the command line, and ends the
/// process with exit 2 when `--e2e` is given but cannot be honoured. A no-op without `--e2e`.
pub fn init() {
    if LOADED.get().is_some() {
        return;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let loaded = match load(&args) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("shell: e2e: {message}");
            std::process::exit(EXIT_REFUSED);
        }
    };
    let _ = LOADED.set(loaded);
    if let Some(loaded) = LOADED.get().and_then(Option::as_ref) {
        start_deadline(Duration::from_millis(loaded.scenario.deadline_ms));
    }
}

fn load(args: &[String]) -> Result<Option<Loaded>, String> {
    let Some(request) = parse_args(args)? else {
        return Ok(None);
    };
    refuse_conflicts(args, std::env::var(PARTIAL_ENV).ok().as_deref())?;
    check_profile(std::env::var_os(USER_DATA_ENV))?;
    let text = std::fs::read_to_string(&request.scenario)
        .map_err(|error| format!("cannot read {}: {error}", request.scenario.display()))?;
    let scenario = scenario::parse(&text, &|name| std::env::var(name).ok())
        .map_err(|error| format!("{}: {error}", request.scenario.display()))?;
    Ok(Some(Loaded { request, scenario }))
}

/// `--e2e <scenario.json> --out <dir>`; `Ok(None)` without `--e2e`
fn parse_args(args: &[String]) -> Result<Option<Request>, String> {
    let mut args = args.iter();
    let (mut scenario, mut out, mut seen) = (None, None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--e2e" => {
                seen = true;
                scenario = Some(args.next().ok_or("--e2e needs <scenario.json>")?);
            }
            "--out" => out = Some(args.next().ok_or("--out needs <dir>")?),
            _ => {}
        }
    }
    if !seen {
        return Ok(None);
    }
    match (scenario, out) {
        (Some(scenario), Some(out)) => Ok(Some(Request {
            scenario: PathBuf::from(scenario),
            out: PathBuf::from(out),
        })),
        _ => Err("--e2e needs --out <dir>".into()),
    }
}

/// `--e2e` runs the whole app, and it is its own mode: no probe, no dialog measurement, no
/// partial channel table
fn refuse_conflicts(args: &[String], partial: Option<&str>) -> Result<(), String> {
    for flag in ["--probe", "--measure-dialog"] {
        if args.iter().any(|arg| arg == flag) {
            return Err(format!("--e2e cannot be combined with {flag}"));
        }
    }
    if partial == Some("1") {
        return Err(format!("--e2e starts the full app: unset {PARTIAL_ENV}"));
    }
    Ok(())
}

/// The profile folder the run may use: it must exist and hold [`PROFILE_MARKER`]
fn check_profile(user_data: Option<OsString>) -> Result<PathBuf, String> {
    let folder = user_data
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!("{USER_DATA_ENV} is not set; the runner needs a throwaway profile")
        })?;
    if !folder.is_dir() {
        return Err(format!(
            "{USER_DATA_ENV} is not a folder: {}",
            folder.display()
        ));
    }
    if !marker_exists(&folder) {
        return Err(format!(
            "{} has no {PROFILE_MARKER}; refusing to run on a profile the workflow did not create",
            folder.display()
        ));
    }
    Ok(folder)
}

fn marker_exists(folder: &Path) -> bool {
    std::fs::metadata(folder.join(PROFILE_MARKER)).is_ok_and(|meta| meta.is_file())
}

/// Starts the runner where the shell is up. A no-op without `--e2e`.
pub fn run(context: &HostContext) -> Result<(), String> {
    let Some(loaded) = LOADED.get().and_then(Option::as_ref) else {
        return Ok(());
    };
    let report = report::Report::create(&loaded.request.out)?;
    let context = context.clone();
    std::thread::Builder::new()
        .name("e2e".into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                steps::execute(&context, &loaded.scenario, &report)
            }));
            let code = outcome.unwrap_or_else(|_| {
                eprintln!("shell: e2e: the runner panicked");
                EXIT_UNRUNNABLE
            });
            context.app.exit(code);
            // an event loop that no longer runs jobs never gets to the exit above; the run's
            // own code must still be the one the process ends with
            std::thread::sleep(EXIT_GRACE);
            std::process::exit(code);
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// A core or UI thread that stops answering is what every hang of the first real runs looked
/// like from outside. From the start of the app (the first window may be what hangs) one
/// thread asks each of them every second and writes the first late answer into the trace: for
/// the core thread with the IPC handler it was inside, and on macOS with the stacks of the
/// process. A no-op without `--e2e`.
pub fn watch(app: &tauri::AppHandle) {
    if request().is_none() {
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("e2e-watch".into())
        .spawn(move || watch_threads(&app));
    if let Err(error) = spawned {
        eprintln!("shell: e2e: cannot start the thread watch: {error}");
    }
}

fn ui_answers(app: &tauri::AppHandle) -> bool {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = send.send(());
    })
    .is_ok()
        && receive.recv_timeout(CORE_LATE).is_ok()
}

fn watch_threads(app: &tauri::AppHandle) {
    let (mut core_late, mut ui_late) = (false, false);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let core = on_core_within(CORE_LATE, |_| ()).is_ok();
        if !core && !core_late {
            let inside =
                crate::ipc::in_flight().unwrap_or_else(|| "no traced IPC handler".to_owned());
            trace!(
                "e2e",
                "the core thread did not answer within {CORE_LATE:?} (inside {inside})"
            );
            capture_stacks("core-late");
        } else if core && core_late {
            trace!("e2e", "the core thread answers again");
        }
        core_late = !core;
        let ui = ui_answers(app);
        if !ui && !ui_late {
            trace!("e2e", "the UI thread ran no job within {CORE_LATE:?}");
            capture_stacks("ui-late");
        } else if ui && ui_late {
            trace!("e2e", "the UI thread runs jobs again");
        }
        ui_late = !ui;
    }
}

/// The deadline counts from the process start, not from the first window: a start that hangs
/// before any window is up (the runner is not there yet) ends here too, with its stacks.
fn start_deadline(deadline: Duration) {
    let spawned = std::thread::Builder::new()
        .name("e2e-deadline".into())
        .spawn(move || {
            std::thread::sleep(deadline);
            eprintln!(
                "shell: e2e: past the deadline of {} ms",
                deadline.as_millis()
            );
            capture_stacks("deadline");
            std::process::exit(EXIT_DEADLINE);
        });
    if let Err(error) = spawned {
        eprintln!("shell: e2e: cannot start the deadline thread: {error}");
        std::process::exit(EXIT_REFUSED);
    }
}

/// What every thread of this process is doing, for a run that stopped answering. On macOS the
/// system `sample` tool writes `<out>/hang-<tag>.txt` (threads are named, so the core thread,
/// the UI thread and the runner can be told apart). Windows cannot read another thread's stack
/// from inside the process; the trace lines of the run log are then the evidence.
pub(super) fn capture_stacks(tag: &str) {
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        use std::time::Instant;
        let Some(request) = request() else {
            return;
        };
        let _ = std::fs::create_dir_all(&request.out);
        let file = request.out.join(format!("hang-{tag}.txt"));
        let child = Command::new("/usr/bin/sample")
            .arg(std::process::id().to_string())
            .args(["3", "-file"])
            .arg(&file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                eprintln!("shell: e2e: cannot run sample: {error}");
                return;
            }
        };
        let end = Instant::now() + SAMPLE_WAIT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    eprintln!("shell: e2e: sample ended ({status}): {}", file.display());
                    return;
                }
                Ok(None) if Instant::now() < end => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                _ => {
                    let _ = child.kill();
                    eprintln!("shell: e2e: sample did not finish within {SAMPLE_WAIT:?}");
                    return;
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!("shell: e2e: no stack capture on this platform ({tag}); the trace is the evidence");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("seuoffice-e2e-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn the_command_line_is_scenario_and_out() {
        let request = parse_args(&args(&["--e2e", "s.json", "--out", "o"]))
            .unwrap()
            .unwrap();
        assert_eq!(request.scenario, PathBuf::from("s.json"));
        assert_eq!(request.out, PathBuf::from("o"));
        assert!(parse_args(&args(&["--out", "o"])).unwrap().is_none());
        assert!(parse_args(&args(&[])).unwrap().is_none());
        assert!(parse_args(&args(&["--e2e", "s.json"])).is_err());
        assert!(parse_args(&args(&["--e2e"])).is_err());
        assert!(parse_args(&args(&["--e2e", "s.json", "--out"])).is_err());
    }

    #[test]
    fn other_test_modes_and_the_partial_table_are_refused() {
        let base = args(&["--e2e", "s.json", "--out", "o"]);
        assert!(refuse_conflicts(&base, None).is_ok());
        assert!(refuse_conflicts(&base, Some("0")).is_ok());
        assert!(refuse_conflicts(&base, Some("1")).is_err());
        let mut probe = base.clone();
        probe.extend(args(&["--probe", "p1"]));
        assert!(refuse_conflicts(&probe, None).is_err());
        let mut measure = base;
        measure.extend(args(&["--measure-dialog", "m.json"]));
        assert!(refuse_conflicts(&measure, None).is_err());
    }

    #[test]
    fn no_profile_variable_means_refusal() {
        assert!(check_profile(None).unwrap_err().contains(USER_DATA_ENV));
        assert!(check_profile(Some(OsString::new())).is_err());
    }

    #[test]
    fn a_missing_folder_means_refusal() {
        let gone = std::env::temp_dir().join("seuoffice-e2e-does-not-exist-9f3a");
        let error = check_profile(Some(gone.into_os_string())).unwrap_err();
        assert!(error.contains("not a folder"), "{error}");
    }

    #[test]
    fn a_folder_without_the_marker_means_refusal() {
        let folder = scratch("unmarked");
        let error = check_profile(Some(folder.clone().into_os_string())).unwrap_err();
        assert!(error.contains(PROFILE_MARKER), "{error}");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_marker_that_is_a_folder_does_not_count() {
        let folder = scratch("marker-dir");
        std::fs::create_dir_all(folder.join(PROFILE_MARKER)).unwrap();
        assert!(check_profile(Some(folder.clone().into_os_string())).is_err());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_marked_folder_is_accepted() {
        let folder = scratch("marked");
        std::fs::write(folder.join(PROFILE_MARKER), b"").unwrap();
        assert_eq!(
            check_profile(Some(folder.clone().into_os_string())),
            Ok(folder.clone())
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
