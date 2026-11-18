//! The runner: the scenario's steps, one at a time, on the real app.
//!
//! A step ends in one of three ways. It ran: `ok: true`, with the value it observed. It ran
//! and did not get what it asked for (a query threw, the document never became ready, no
//! dialog took the answer): `ok: false` with the error, and the runner goes on, because the
//! checker decides what that means. It could not run at all (the OS refused the input, the
//! input source is not installed, there is no window): `ok: false`, and the runner stops with
//! exit 3, as nothing after it would mean anything.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shell_contract::ids::ViewId;
use tauri::{Manager, Webview, Window};

use super::EXIT_UNRUNNABLE;
use super::dialog::{self, Answer};
use super::report::{Report, StepRecord};
use super::scenario::{Scenario, Shortcut, Step, typed_keys};
use crate::host::HostContext;
use crate::host::contents::page_error_text;
use crate::host::shell_tabs::shell_tabs;
use crate::host::w::on_core_within;
use crate::native::document::Life;
use crate::native::{CaptureSpec, QuietSpec, WebviewExt};
use crate::trace::trace;

#[cfg(target_os = "macos")]
use super::input_mac as input;
#[cfg(windows)]
use super::input_win as input;

const POLL: Duration = Duration::from_millis(50);
/// the shell window and its first tab are up within this, or nothing can run
const SHELL_START_TIMEOUT: Duration = Duration::from_secs(120);
/// the core thread answers the runner's questions within this, or it is stuck in a job
const CORE_STALL: Duration = Duration::from_secs(90);
const QUIET_FOR_MS: u32 = 500;
const QUIET_TIMEOUT_MS: u32 = 10_000;
/// a file written by an export must keep its size this long before it counts as complete
const FILE_STABLE: Duration = Duration::from_millis(600);
/// after a quit the app ends by itself; the runner waits this long for it
const QUIT_WAIT: Duration = Duration::from_secs(60);

enum StepError {
    /// ran, did not get what it asked for
    Failed(String),
    /// could not run
    Unrunnable(String),
}

impl StepError {
    fn into_text(self) -> String {
        match self {
            Self::Failed(text) | Self::Unrunnable(text) => text,
        }
    }
}

fn failed(message: impl ToString) -> StepError {
    StepError::Failed(message.to_string())
}

fn unrunnable(message: impl ToString) -> StepError {
    StepError::Unrunnable(message.to_string())
}

type StepResult = Result<Option<Value>, StepError>;

/// The active tab as the shell's tab state names it
struct Active {
    view: ViewId,
    kind: &'static str,
    title: Option<String>,
    file: Option<String>,
}

impl Active {
    fn describe(&self) -> String {
        format!(
            "{} view {} title {:?} file {:?}",
            self.kind, self.view.0, self.title, self.file
        )
    }
}

/// The active tab's editor: its view, webview and the window that holds it
struct Target {
    kind: &'static str,
    webview: Webview,
    window: Window,
}

/// The tab that `open` or `newDocument` is about to bring up, and how `waitReady` knows it
enum TabSwitch {
    /// the tab of this file, whether it opens now or was open already
    File(PathBuf),
    /// a new document has no file: any tab but the one that was active (`None`: none was)
    Other(Option<ViewId>),
}

struct Runner<'a> {
    context: &'a HostContext,
    switch: Option<TabSwitch>,
    /// a `quit` shortcut was sent: the app is ending, and what follows is part of that
    quitting: bool,
}

/// Runs the scenario and returns the process's exit code
pub(super) fn execute(context: &HostContext, scenario: &Scenario, report: &Report) -> i32 {
    let mut runner = Runner {
        context,
        switch: None,
        quitting: false,
    };
    if let Err(error) = runner.wait_shell() {
        eprintln!("shell: e2e: {error}");
        return EXIT_UNRUNNABLE;
    }
    for (index, step) in scenario.steps.iter().enumerate() {
        let started = Instant::now();
        trace!("e2e", "step {index} ({}) begins", step.op());
        let mut record = StepRecord {
            step: index,
            op: step.op(),
            name: step.name().map(str::to_owned),
            ok: true,
            ms: 0,
            value: None,
            error: None,
        };
        // the app ends while the tail of a quit runs, and the line must be on disk before it
        let before_quit = matches!(step, Step::Shortcut(Shortcut::Quit))
            || (runner.quitting && matches!(step, Step::Dialog { .. }));
        if before_quit {
            record.value = Some(match step {
                Step::Dialog { answer, .. } => json!({ "answered": answer.label(), "quit": true }),
                _ => json!({ "requested": true }),
            });
            if let Err(error) = report.write(&record) {
                eprintln!("shell: e2e: {error}");
                return EXIT_UNRUNNABLE;
            }
        }
        let outcome = runner.run(step);
        record.ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        trace!(
            "e2e",
            "step {index} ({}) ended after {} ms, ok {}",
            record.op,
            record.ms,
            outcome.is_ok()
        );
        let stop = matches!(outcome, Err(StepError::Unrunnable(_)));
        if before_quit {
            if let Err(StepError::Failed(error) | StepError::Unrunnable(error)) = &outcome {
                eprintln!("shell: e2e: step {index} ({}): {error}", record.op);
            }
            if stop {
                return EXIT_UNRUNNABLE;
            }
            continue;
        }
        match outcome {
            Ok(value) => record.value = value,
            Err(StepError::Failed(error) | StepError::Unrunnable(error)) => {
                record.ok = false;
                record.error = Some(error);
            }
        }
        if let Err(error) = report.write(&record) {
            eprintln!("shell: e2e: {error}");
            return EXIT_UNRUNNABLE;
        }
        if stop {
            return EXIT_UNRUNNABLE;
        }
    }
    if runner.quitting {
        // a quit that saves first must not be cut short by the runner's own exit
        sleep(QUIT_WAIT);
        eprintln!("shell: e2e: the app did not end {QUIT_WAIT:?} after the quit");
    }
    0
}

impl Runner<'_> {
    /// The active tab. On the core thread, where the tab state lives; a core that does not
    /// answer ends the run there, with the stacks of the process, instead of leaving it to the
    /// scenario's deadline.
    fn active(&self) -> Result<Option<Active>, StepError> {
        let asked = on_core_within(CORE_STALL, |provided| {
            let tabs = shell_tabs(provided)?;
            let kind = tabs.menu_kind()?;
            let view = tabs.active_editor(kind)?;
            let (title, file) = tabs
                .describe_view(view)
                .map_or((None, None), |(title, file)| (Some(title), file));
            Some(Active {
                view,
                kind: kind.as_str(),
                title,
                file,
            })
        });
        match asked {
            Ok(active) => Ok(active.flatten()),
            Err(_) => {
                let inside = crate::ipc::in_flight()
                    .unwrap_or_else(|| "no traced IPC handler".to_owned());
                let message = format!(
                    "the core thread did not answer within {CORE_STALL:?} (inside {inside})"
                );
                trace!("e2e", "{message}");
                super::capture_stacks("core-stalled");
                Err(unrunnable(message))
            }
        }
    }

    fn webview_of(&self, view: ViewId) -> Result<Webview, String> {
        let label = self
            .context
            .views
            .label_of_view(view)
            .ok_or_else(|| format!("view {} has no webview", view.0))?;
        self.context
            .app
            .get_webview(&label)
            .ok_or_else(|| format!("there is no webview {label}"))
    }

    fn target(&self) -> Result<Target, StepError> {
        let active = self
            .active()?
            .ok_or_else(|| failed("there is no active tab"))?;
        let webview = self.webview_of(active.view).map_err(failed)?;
        let window = webview.window();
        Ok(Target {
            kind: active.kind,
            webview,
            window,
        })
    }

    /// The document of the webview went through the bridge's render-ready (registered, loaded,
    /// fonts ready): the app's own signal that the page is up
    fn is_ready(webview: &Webview) -> bool {
        webview
            .native_browser()
            .ok()
            .and_then(|browser| browser.registered_context())
            .is_some_and(|context| matches!(context.0.life(), Life::Ready(_)))
    }

    /// No resource in flight for a moment. Whether the page ever gets quiet is not a failure.
    fn settle(webview: &Webview) -> bool {
        let Ok(page) = webview.page() else {
            return false;
        };
        let timeout = Duration::from_millis(u64::from(QUIET_TIMEOUT_MS) + 5_000);
        page.blocking(timeout, |done| {
            page.wait_quiet(
                QuietSpec {
                    quiet_ms: QUIET_FOR_MS,
                    timeout_ms: QUIET_TIMEOUT_MS,
                },
                None,
                done,
            );
        })
        .is_ok()
    }

    /// Before the first step: the shell window has its first tab, and that page is ready
    fn wait_shell(&self) -> Result<(), String> {
        let deadline = Instant::now() + SHELL_START_TIMEOUT;
        loop {
            if let Some(active) = self.active().map_err(StepError::into_text)?
                && let Ok(webview) = self.webview_of(active.view)
                && Self::is_ready(&webview)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the shell window was not ready within {SHELL_START_TIMEOUT:?}"
                ));
            }
            sleep(POLL);
        }
    }

    /// `open` waits for the tab of its file, `newDocument` for any tab but the one that was
    /// active. A tab that was already showing the file counts: opening it only switches to it.
    fn begin_switch(&mut self, file: Option<PathBuf>) -> Result<(), StepError> {
        self.switch = Some(match file {
            Some(path) => TabSwitch::File(path),
            None => TabSwitch::Other(self.active()?.map(|active| active.view)),
        });
        Ok(())
    }

    fn is_wanted(&self, active: &Active) -> bool {
        match &self.switch {
            None => true,
            Some(TabSwitch::File(expected)) => active
                .file
                .as_deref()
                .is_some_and(|file| same_file(file, expected)),
            Some(TabSwitch::Other(from)) => *from != Some(active.view),
        }
    }

    fn wait_ready(&mut self, timeout: Duration) -> StepResult {
        let deadline = Instant::now() + timeout;
        let mut last = None;
        loop {
            if let Some(active) = self.active()? {
                if self.is_wanted(&active)
                    && let Ok(webview) = self.webview_of(active.view)
                    && Self::is_ready(&webview)
                {
                    self.switch = None;
                    let quiet = Self::settle(&webview);
                    return Ok(Some(json!({
                        "kind": active.kind,
                        "view": active.view.0,
                        "title": active.title,
                        "file": active.file,
                        "quiet": quiet,
                    })));
                }
                last = Some(active);
            }
            if Instant::now() >= deadline {
                return Err(failed(format!(
                    "the active tab's document was not ready within {} ms (active tab: {})",
                    timeout.as_millis(),
                    last.as_ref().map_or_else(|| "none".to_owned(), Active::describe)
                )));
            }
            sleep(POLL);
        }
    }

    fn run(&mut self, step: &Step) -> StepResult {
        match step {
            Step::Open { path } => {
                if !path.exists() {
                    return Err(failed(format!("{} does not exist", path.display())));
                }
                self.begin_switch(Some(path.clone()))?;
                crate::host::os::open_paths(vec![path.to_string_lossy().into_owned()], true);
                Ok(Some(json!({ "path": path.display().to_string() })))
            }
            Step::NewDocument { kind } => {
                self.begin_switch(None)?;
                let home_kind = kind.home_kind();
                let created = on_core_within(CORE_STALL, move |provided| {
                    shell_tabs(provided).is_some_and(|tabs| tabs.new_document(home_kind))
                });
                match created {
                    Ok(Some(true)) => Ok(Some(json!({ "kind": home_kind }))),
                    Ok(_) => Err(failed(format!("the shell cannot create a {home_kind} tab"))),
                    Err(_) => Err(unrunnable(format!(
                        "the core thread did not answer within {CORE_STALL:?}"
                    ))),
                }
            }
            Step::WaitReady { timeout_ms } => self.wait_ready(Duration::from_millis(*timeout_ms)),
            Step::Type { text } => {
                let target = self.target()?;
                target.webview.set_focus().map_err(failed)?;
                let keys = typed_keys(text);
                input::type_text(&self.context.ui, &target.window, &keys).map_err(unrunnable)?;
                Ok(Some(json!({ "chars": keys.len(), "kind": target.kind })))
            }
            Step::Ime { keys } => {
                let target = self.target()?;
                target.webview.set_focus().map_err(failed)?;
                input::ime_ko_2set(&self.context.ui, &target.window, keys)
                    .map(Some)
                    .map_err(unrunnable)
            }
            Step::Shortcut(shortcut) => {
                let target = self.target()?;
                input::shortcut(&self.context.ui, &target.window, *shortcut).map_err(unrunnable)?;
                if *shortcut == Shortcut::Quit {
                    self.quitting = true;
                }
                Ok(Some(json!({ "kind": target.kind })))
            }
            Step::Clipboard { text, html } => self.set_clipboard(text.clone(), html.clone()),
            Step::Query { js, timeout_ms, .. } => {
                self.query(js, Duration::from_millis(*timeout_ms))
            }
            Step::Dialog { answer, timeout_ms } => {
                let ticket = dialog::push(answer.clone());
                if self.quitting {
                    // the record is already written; the app may end any moment
                    dialog::wait_consumed(ticket, Duration::from_millis(*timeout_ms));
                    return Ok(None);
                }
                if dialog::wait_consumed(ticket, Duration::from_millis(*timeout_ms)) {
                    Ok(Some(json!({ "answered": answer.label() })))
                } else {
                    dialog::discard(ticket);
                    Err(failed(format!(
                        "no dialog took the answer {} within {timeout_ms} ms",
                        answer.label()
                    )))
                }
            }
            Step::ExportPdf { path, timeout_ms } => {
                self.export_pdf(path, Duration::from_millis(*timeout_ms))
            }
            Step::Screenshot { name } => self.screenshot(name),
            Step::Wait { ms } => {
                sleep(Duration::from_millis(*ms));
                Ok(None)
            }
        }
    }

    fn set_clipboard(&self, text: Option<String>, html: Option<String>) -> StepResult {
        let written = text.as_ref().map(String::len);
        crate::host::on_ui(&self.context.ui, move || -> Result<(), String> {
            use clipboard_rs::{Clipboard as _, ClipboardContent, ClipboardContext};
            let clipboard = ClipboardContext::new().map_err(|error| error.to_string())?;
            let mut contents = Vec::new();
            if let Some(html) = html {
                contents.push(ClipboardContent::Html(html));
            }
            if let Some(text) = text {
                contents.push(ClipboardContent::Text(text));
            }
            clipboard.set(contents).map_err(|error| error.to_string())
        })
        .map_err(failed)?;
        Ok(Some(json!({ "textBytes": written })))
    }

    /// The expression's value, awaited when it is a promise, as JSON
    fn query(&self, js: &str, timeout: Duration) -> StepResult {
        let target = self.target()?;
        let page = target.webview.page().map_err(failed)?;
        // the page runs one expression; a trailing `;` is a syntax error on WKWebView
        let expression = js.trim().trim_end_matches(';').to_owned();
        let value = page
            .blocking(timeout, |done| page.evaluate(&expression, true, None, done))
            .map_err(|error| failed(page_error_text(error)))?;
        Ok(Some(value))
    }

    fn export_pdf(&self, path: &Path, timeout: Duration) -> StepResult {
        // a leftover file would pass for the export
        let _ = fs::remove_file(path);
        let ticket = dialog::push(Answer::Path(path.to_path_buf()));
        let outcome = crate::host::menu::e2e_export_pdf()
            .map_err(failed)
            .and_then(|()| wait_for_pdf(path, timeout).map_err(failed));
        // an export that never asked for a path must not leave its answer for a later dialog
        dialog::discard(ticket);
        let bytes = outcome?;
        Ok(Some(
            json!({ "path": path.display().to_string(), "bytes": bytes }),
        ))
    }

    fn screenshot(&self, name: &str) -> StepResult {
        let target = self.target()?;
        let out = super::request()
            .map(|request| request.out.clone())
            .ok_or_else(|| unrunnable("the e2e request is gone"))?;
        let path = out.join(format!("{name}.png"));
        let window_id = input::window_id(&self.context.ui, &target.window).map_err(unrunnable)?;
        let source = match capture_window(window_id, &path) {
            Ok(()) => "window",
            Err(window_error) => {
                // the OS may refuse window capture (macOS screen recording permission);
                // the page's own capture needs none
                let png = crate::host::contents::capture_png(
                    &target.webview,
                    CaptureSpec::default(),
                )
                .map_err(|page_error| {
                    failed(format!(
                        "window capture failed ({window_error}); page capture failed ({page_error})"
                    ))
                })?;
                fs::write(&path, png).map_err(failed)?;
                "webview"
            }
        };
        let bytes = fs::metadata(&path).map_err(failed)?.len();
        Ok(Some(
            json!({ "path": path.display().to_string(), "bytes": bytes, "source": source }),
        ))
    }
}

/// Whether the tab's file is the one `open` named, both as the file system resolves them
fn same_file(tab: &str, expected: &Path) -> bool {
    let resolved = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved(Path::new(tab)) == resolved(expected)
}

/// The app window as a PNG, through the screen capture crate the shell already uses
fn capture_window(window_id: u32, path: &Path) -> Result<(), String> {
    let windows = xcap::Window::all().map_err(|error| error.to_string())?;
    for window in windows {
        if window.id().map_err(|error| error.to_string())? != window_id {
            continue;
        }
        let image = window.capture_image().map_err(|error| error.to_string())?;
        if image.width() == 0 || image.height() == 0 {
            return Err("the window image is empty".into());
        }
        return image
            .save_with_format(path, image::ImageFormat::Png)
            .map_err(|error| error.to_string());
    }
    Err(format!("window {window_id} is not in the OS window list"))
}

/// Waits for a complete PDF at `path`: it exists, stopped growing, starts with `%PDF-`
fn wait_for_pdf(path: &Path, timeout: Duration) -> Result<u64, String> {
    let deadline = Instant::now() + timeout;
    let mut last: Option<(u64, Instant)> = None;
    loop {
        if let Ok(meta) = fs::metadata(path)
            && meta.len() > 0
        {
            let length = meta.len();
            match last {
                Some((previous, since)) if previous == length => {
                    if since.elapsed() >= FILE_STABLE {
                        return check_pdf_header(path).map(|()| length);
                    }
                }
                _ => last = Some((length, Instant::now())),
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no PDF appeared at {} within {} ms",
                path.display(),
                timeout.as_millis()
            ));
        }
        sleep(POLL * 4);
    }
}

fn check_pdf_header(path: &Path) -> Result<(), String> {
    let mut head = [0u8; 5];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if &head == b"%PDF-" {
        Ok(())
    } else {
        Err(format!("{} is not a PDF", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let folder =
            std::env::temp_dir().join(format!("seuoffice-e2e-steps-{name}-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn a_complete_pdf_is_recognised_by_its_header_and_size() {
        let folder = scratch("pdf");
        let path = folder.join("a.pdf");
        fs::write(&path, b"%PDF-1.7\n%%EOF").unwrap();
        assert_eq!(
            wait_for_pdf(&path, Duration::from_secs(5)),
            Ok(fs::metadata(&path).unwrap().len())
        );
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_file_that_is_not_a_pdf_is_refused() {
        let folder = scratch("notpdf");
        let path = folder.join("a.pdf");
        fs::write(&path, b"<html></html>").unwrap();
        assert!(
            wait_for_pdf(&path, Duration::from_secs(5))
                .unwrap_err()
                .contains("not a PDF")
        );
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_pdf_that_never_appears_times_out() {
        let folder = scratch("missing");
        let error = wait_for_pdf(&folder.join("none.pdf"), Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("no PDF appeared"), "{error}");
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_tab_file_matches_the_opened_path_however_it_is_spelled() {
        let folder = scratch("same");
        let file = folder.join("a.drawio");
        let other = folder.join("b.drawio");
        fs::write(&file, b"x").unwrap();
        fs::write(&other, b"x").unwrap();
        let detour = folder.join(".").join("a.drawio");
        assert!(same_file(&detour.to_string_lossy(), &file));
        assert!(!same_file(&other.to_string_lossy(), &file));
        assert!(!same_file("", &file));
        fs::remove_dir_all(&folder).unwrap();
    }
}
