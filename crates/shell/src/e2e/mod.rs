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
//! 3 when a step could not run (the runner stops there), 124 past the scenario's deadline.
//!
//! | op | how |
//! |---|---|
//! | `open` | `host::os::open_paths`, the path of macOS open-file events and the single-instance argv |
//! | `newDocument` | `ShellTabs::new_document`, as the Home tiles and the New menu do |
//! | `waitReady` | the active tab's document reached the bridge's render-ready state, then the page is quiet |
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

/// The file the workflow puts in its throwaway profile folder
pub const PROFILE_MARKER: &str = ".seuoffice-e2e-profile";

const EXIT_REFUSED: i32 = 2;
const EXIT_UNRUNNABLE: i32 = 3;
const EXIT_DEADLINE: i32 = 124;

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
    let deadline = Duration::from_millis(loaded.scenario.deadline_ms);
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
        })
        .map_err(|error| error.to_string())?;
    std::thread::Builder::new()
        .name("e2e-deadline".into())
        .spawn(move || {
            std::thread::sleep(deadline);
            eprintln!(
                "shell: e2e: past the deadline of {} ms",
                deadline.as_millis()
            );
            std::process::exit(EXIT_DEADLINE);
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
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
