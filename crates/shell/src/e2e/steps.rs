//! The runner: the scenario's steps, one at a time, on the real app.
//!
//! A step ends in one of three ways. It ran: `ok: true`, with the value it observed. It ran
//! and did not get what it asked for (a query threw, the document never became ready, no
//! dialog took the answer): `ok: false` with the error, and the runner goes on, because the
//! checker decides what that means. It could not run at all (the OS refused the input, the
//! input source is not installed, there is no window): `ok: false`, and the runner stops with
//! exit 3, as nothing after it would mean anything.

use std::collections::HashSet;
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
use super::scenario::{Scenario, Shortcut, Step, TypedKey, typed_keys};
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
/// how often a key the page did not record is sent before it counts as lost
const KEY_TRIES: u32 = 4;
/// how long a key the page has not recorded yet is given before it counts as lost
const KEY_LATE: Duration = Duration::from_millis(1500);
/// the shell window and its first tab are up within this, or nothing can run
const SHELL_START_TIMEOUT: Duration = Duration::from_secs(120);
/// the core thread answers the runner's questions within this, or it is stuck in a job
const CORE_STALL: Duration = Duration::from_secs(90);
const QUIET_FOR_MS: u32 = 500;
/// a `prep-` query that found nothing to act on is asked again after this
const PREP_RETRY: Duration = Duration::from_millis(250);
/// the page answers a focus or flush question within this, or the answer is dropped
const FOCUS_QUERY: Duration = Duration::from_secs(3);
/// the page has the OS focus within this of the request, or the keys go out anyway
const FOCUS_WAIT: Duration = Duration::from_secs(5);
/// a view answers the page error question within this, or it is skipped
const PAGE_ERRORS_QUERY: Duration = Duration::from_secs(1);
const QUIET_TIMEOUT_MS: u32 = 10_000;
/// a file written by an export must keep its size this long before it counts as complete
const FILE_STABLE: Duration = Duration::from_millis(600);
/// after a quit the app ends by itself; the runner waits this long for it
const QUIT_WAIT: Duration = Duration::from_secs(60);
/// a tab is gone within this of its Close Tab, or the key closed nothing
const CLOSE_WAIT: Duration = Duration::from_secs(10);

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
    /// page errors already written to the trace
    reported: HashSet<String>,
}

/// Runs the scenario and returns the process's exit code
pub(super) fn execute(context: &HostContext, scenario: &Scenario, report: &Report) -> i32 {
    let mut runner = Runner {
        context,
        switch: None,
        quitting: false,
        reported: HashSet::new(),
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
        if !runner.quitting {
            runner.report_page_errors();
        }
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
                let inside =
                    crate::ipc::in_flight().unwrap_or_else(|| "no traced IPC handler".to_owned());
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

    /// The active tab's editor. On the Home tab, which has no editor view, the shell window's
    /// own page: the launch recovery list is answered there.
    fn target(&self) -> Result<Target, StepError> {
        let (kind, view) = match self.active()? {
            Some(active) => (active.kind, active.view),
            None => {
                let shell = self
                    .context
                    .views
                    .marked_main()
                    .ok_or_else(|| failed("there is no active tab"))?;
                ("home", shell.view)
            }
        };
        let webview = self.webview_of(view).map_err(failed)?;
        let window = webview.window();
        Ok(Target {
            kind,
            webview,
            window,
        })
    }

    /// A Close Tab is done when the tab it was sent to is no longer the active one, and fails
    /// when it still is after `CLOSE_WAIT`. A scenario that opens the file again reads a
    /// reopened document only then: `waitReady` also takes a tab of that file that was open
    /// already. On Windows the key closed nothing (the second `waitReady` named the first
    /// one's view in every run), so each read "after reopen" there was of the page that had
    /// never been closed.
    fn wait_closed(&self, view: ViewId) -> Result<(), StepError> {
        let end = Instant::now() + CLOSE_WAIT;
        loop {
            if self.active()?.map(|active| active.view) != Some(view) {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err(failed(format!(
                    "the tab of view {} is still the active tab {} ms after Close Tab: it did \
                     not close",
                    view.0,
                    CLOSE_WAIT.as_millis()
                )));
            }
            sleep(POLL * 2);
        }
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

    /// Before the first step: the shell window is up and its page is ready. The first tab is the
    /// Home tab, which may name no editor view, so the window's own view is what counts then.
    fn wait_shell(&self) -> Result<(), String> {
        let deadline = Instant::now() + SHELL_START_TIMEOUT;
        loop {
            let view = match self.active().map_err(StepError::into_text)? {
                Some(active) => Some(active.view),
                None => self.context.views.marked_main().map(|sender| sender.view),
            };
            if let Some(view) = view
                && let Ok(webview) = self.webview_of(view)
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

    /// The editors with a control API (`window.__seuofficeControl`) install it, and answer
    /// `not_ready`, after the bridge's render-ready: the document is still loading into the
    /// engine. Scenarios query through that API, so `waitReady` waits for it to answer. The
    /// state it ended in is the value: `ready`, `not_ready` (never answered within the step's
    /// time), `absent` (never installed) or `none` (this editor has no control API).
    fn wait_control(webview: &Webview, kind: &str, deadline: Instant) -> &'static str {
        if !matches!(kind, "docs" | "sheets" | "slides" | "pdf") {
            return "none";
        }
        let Ok(page) = webview.page() else {
            return "absent";
        };
        let probe = "(async () => { const c = window.__seuofficeControl; \
             if (typeof c !== 'function') return 'absent'; \
             try { const r = await c({ cmd: 'selection' }); \
             return r && r.status === 'not_ready' ? 'not_ready' : 'ready' } \
             catch (error) { return 'ready' } })()";
        let mut state = "absent";
        loop {
            let answer = page
                .blocking(FOCUS_QUERY, |done| page.evaluate(probe, true, None, done))
                .ok();
            state = match answer.as_ref().and_then(Value::as_str) {
                Some("ready") => return "ready",
                Some("not_ready") => "not_ready",
                _ => state,
            };
            if Instant::now() >= deadline {
                return state;
            }
            sleep(POLL);
        }
    }

    /// What a page that did not become ready was doing: its last resource loads (a fetch that
    /// never ended has no entry), what it shows, and what its recorder caught
    fn page_state(webview: &Webview) -> Option<Value> {
        let page = webview.page().ok()?;
        let expression = "(() => ({ \
             resources: performance.getEntriesByType('resource').slice(-8).map((e) => \
             e.name.split('/').pop().slice(0, 40) + ' ' + Math.round(e.startTime) + '+' + \
             Math.round(e.duration) + 'ms ' + e.initiatorType), \
             now: Math.round(performance.now()), \
             text: document.body ? document.body.innerText.replace(/\\s+/g, ' ').slice(-200) : null, \
             errors: window.__e2eErrors || [] }))()";
        page.blocking(FOCUS_QUERY, |done| {
            page.evaluate(expression, false, None, done)
        })
        .ok()
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
                    let control = Self::wait_control(&webview, active.kind, deadline);
                    let mut value = json!({
                        "kind": active.kind,
                        "view": active.view.0,
                        "title": active.title,
                        "file": active.file,
                        "quiet": quiet,
                        "control": control,
                    });
                    if matches!(control, "not_ready" | "absent") {
                        // the editor never came up: what the page last loaded and shows
                        value["page"] = json!(Self::page_state(&webview));
                    }
                    return Ok(Some(value));
                }
                last = Some(active);
            }
            if Instant::now() >= deadline {
                return Err(failed(format!(
                    "the active tab's document was not ready within {} ms (active tab: {})",
                    timeout.as_millis(),
                    last.as_ref()
                        .map_or_else(|| "none".to_owned(), Active::describe)
                )));
            }
            sleep(POLL);
        }
    }

    /// What the pages recorded about their own errors (see `page_error_script`), in every view
    /// and its same-origin frames, written once each to the trace. Best effort: a view that
    /// does not answer within a second is skipped.
    fn report_page_errors(&mut self) {
        let expression = "(() => { const out = []; \
             const grab = (w, tag) => { try { for (const e of (w.__e2eErrors || [])) out.push(tag + ' ' + e) } catch (error) {} }; \
             grab(window, 'top'); \
             for (const f of document.querySelectorAll('iframe')) { try { grab(f.contentWindow, 'frame') } catch (error) {} } \
             return out })()";
        for (label, webview) in self.context.app.webviews() {
            if !label.starts_with("view-") {
                continue;
            }
            let Ok(page) = webview.page() else { continue };
            let Ok(value) = page.blocking(PAGE_ERRORS_QUERY, |done| {
                page.evaluate(expression, false, None, done)
            }) else {
                continue;
            };
            for line in value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if self.reported.insert(format!("{label} {line}")) {
                    let short: String = line.chars().take(400).collect();
                    trace!("pageerr", "{label}: {short}");
                }
            }
        }
    }

    fn run(&mut self, step: &Step) -> StepResult {
        match step {
            Step::Open { path } => {
                if !path.exists() {
                    return Err(failed(format!("{} does not exist", path.display())));
                }
                // The scenario spells paths with `/`; the OS hands the app its own spelling
                // (`\` on Windows), which is what a tab's file, a save and a recent entry see.
                let native: PathBuf = path.components().collect();
                self.begin_switch(Some(native.clone()))?;
                crate::host::os::open_paths(vec![native.to_string_lossy().into_owned()], true);
                Ok(Some(json!({ "path": native.display().to_string() })))
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
                let focus = Self::focus_page(&target.webview)?;
                let keys = typed_keys(text);
                Self::reset_record(&target.webview, "__e2eText");
                // One key at a time: the page must have handled a key before the next arrives,
                // or an editor under load drops characters out of the text. Windows also loses
                // injected keys before they reach the page (S82 runs: `diagram` arrived as
                // `dagram`, `SEU-E2E-hwp` without its hyphen). Once the page has been seen to
                // record keys in this step, a key it does not record is sent again, and the value
                // says how many were resent, so the loss stays visible.
                let mut observed = false;
                let mut resent = 0_u32;
                let mut unconfirmed = 0_u32;
                for key in &keys {
                    let want = Self::key_name(key);
                    for attempt in 0..KEY_TRIES {
                        if want.is_some() {
                            Self::reset_keys(&target.webview);
                        }
                        input::type_text(
                            &self.context.ui,
                            &target.window,
                            std::slice::from_ref(key),
                            &|| Self::flush_input(&target.webview),
                        )
                        .map_err(unrunnable)?;
                        let Some(want) = &want else { break };
                        Self::flush_input(&target.webview);
                        let mut arrival = Self::key_arrived(&target.webview, want);
                        // a key still on its way would be typed twice by a resend, and a second
                        // Tab or Enter undoes the first: look again for a while before that
                        let late = Instant::now() + KEY_LATE;
                        while matches!(arrival, Some((false, _))) && Instant::now() < late {
                            sleep(POLL * 2);
                            Self::flush_input(&target.webview);
                            arrival = Self::key_arrived(&target.webview, want);
                        }
                        match arrival {
                            Some((true, _)) => {
                                observed = true;
                                break;
                            }
                            // lost, or gone into a frame the recorder cannot read: only a page
                            // that has recorded keys in this step tells the two apart
                            Some((false, blind)) if !blind || observed => {
                                if attempt + 1 == KEY_TRIES {
                                    unconfirmed += 1;
                                } else {
                                    resent += 1;
                                }
                            }
                            _ => {
                                unconfirmed += 1;
                                break;
                            }
                        }
                    }
                }
                Self::flush_input(&target.webview);
                let after = Self::focus_state(&target.webview);
                Ok(Some(json!({
                    "chars": keys.len(),
                    "kind": target.kind,
                    "focusBefore": focus,
                    "focusAfter": after,
                    "resent": resent,
                    "unconfirmed": unconfirmed,
                    "inserted": Self::inserted(&target.webview),
                })))
            }
            Step::Ime { keys, prime } => {
                let target = self.target()?;
                Self::focus_page(&target.webview)?;
                // `true` clears the page's key record; `false` says whether the last key the page
                // saw was the IME's `Process` (a letter that came through is not composing)
                let composing = |reset: bool| -> Option<bool> {
                    if reset {
                        Self::reset_keys(&target.webview);
                        None
                    } else {
                        Self::flush_input(&target.webview);
                        Self::key_arrived(&target.webview, "<Process>").map(|(seen, _)| seen)
                    }
                };
                let value =
                    input::ime_ko_2set(&self.context.ui, &target.window, keys, *prime, &composing)
                        .map_err(unrunnable)?;
                Self::flush_input(&target.webview);
                Ok(Some(value))
            }
            Step::Shortcut(shortcut) => {
                // the tab a Close Tab is sent to, read before the key
                let closing = match shortcut {
                    Shortcut::CloseTab => self.active()?.map(|active| active.view),
                    _ => None,
                };
                let target = self.target()?;
                let focus = Self::focus_page(&target.webview)?;
                input::shortcut(&self.context.ui, &target.window, *shortcut).map_err(unrunnable)?;
                if *shortcut == Shortcut::Quit {
                    self.quitting = true;
                }
                if let Some(view) = closing {
                    self.wait_closed(view)?;
                }
                Ok(Some(json!({ "kind": target.kind, "focusBefore": focus })))
            }
            Step::Clipboard { text, html } => self.set_clipboard(text.clone(), html.clone()),
            Step::Query {
                name,
                js,
                timeout_ms,
            } => {
                let timeout = Duration::from_millis(*timeout_ms);
                if name.starts_with("prep-") {
                    self.query_prep(js, timeout)
                } else {
                    self.query(js, timeout)
                }
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
            Step::SaveAsOwned { path, timeout_ms } => {
                self.save_as_owned(path, Duration::from_millis(*timeout_ms))
            }
            Step::Screenshot { name } => self.screenshot(name),
            Step::Click {
                selector,
                fx,
                fy,
                timeout_ms,
            } => self.click(selector, *fx, *fy, Duration::from_millis(*timeout_ms)),
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

    /// Whether the page has OS focus, which element holds it, and how often its window has
    /// gained and lost the focus since it loaded, as the page itself says. The counts of two
    /// states apart tell a focus that went back and forth in between: an editor that ends an
    /// edit when it loses the focus keeps none then.
    fn focus_state(webview: &Webview) -> Option<Value> {
        let page = webview.page().ok()?;
        let expression = "(() => { const a = document.activeElement; return { hasFocus: \
             document.hasFocus(), active: a ? a.tagName + (a.className ? '.' + \
             String(a.className).split(' ')[0] : '') : null, \
             window: window.__e2eFocus || null } })()";
        page.blocking(FOCUS_QUERY, |done| {
            page.evaluate(expression, false, None, done)
        })
        .ok()
    }

    /// Keys go to the first responder. `set_focus` is asked of the UI thread and answers before
    /// it has run, so the first keys of a `type` were lost: wait until the page says it has the
    /// focus (the last state is returned either way; the checker sees what the keys met).
    ///
    /// A page that has the focus already is left alone. Focusing it again can move the focus
    /// inside the page on Windows (WebView2's `MoveFocus`), and an editor that ends an edit
    /// when it loses the focus (a mind map topic, a slide's text box) would then lose what the
    /// step before began.
    fn focus_page(webview: &Webview) -> Result<Option<Value>, StepError> {
        let state = Self::focus_state(webview);
        let focused = |state: &Option<Value>| {
            state
                .as_ref()
                .and_then(|state| state.get("hasFocus"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        if focused(&state) {
            return Ok(state);
        }
        webview.set_focus().map_err(failed)?;
        let end = Instant::now() + FOCUS_WAIT;
        loop {
            let state = Self::focus_state(webview);
            let focused = state
                .as_ref()
                .and_then(|state| state.get("hasFocus"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if focused || Instant::now() >= end {
                return Ok(state);
            }
            sleep(POLL);
        }
    }

    /// The page has handled the key events posted so far: a round trip that ends after a frame
    /// and a timer, which the engine's own input handling queues behind
    fn flush_input(webview: &Webview) {
        let Ok(page) = webview.page() else { return };
        let expression = "new Promise(done => requestAnimationFrame(() => setTimeout(done, 50)))";
        let _ = page.blocking(FOCUS_QUERY, |done| {
            page.evaluate(expression, true, None, done)
        });
    }

    /// Whether the page's own `keydown` listeners saw `want` as the last key since the last
    /// [`Self::reset_keys`], in its top document or in a same-origin iframe; and whether an
    /// iframe was out of the recorder's reach (cross-origin), where the key may have gone
    /// instead. `None` when the page cannot be asked.
    fn key_arrived(webview: &Webview, want: &str) -> Option<(bool, bool)> {
        let page = webview.page().ok()?;
        let expression = "(() => { let blind = false; \
             const last = (w) => { try { const keys = w.__e2eKeys || []; return keys.length ? keys[keys.length - 1] : null } catch (error) { blind = true; return null } }; \
             const seen = [last(window), ...[...document.querySelectorAll('iframe')].map((f) => last(f.contentWindow))]; \
             return { seen, blind } })()";
        let answer = page
            .blocking(FOCUS_QUERY, |done| {
                page.evaluate(expression, false, None, done)
            })
            .ok()?;
        let arrived = answer
            .get("seen")?
            .as_array()?
            .iter()
            .any(|entry| entry.as_str() == Some(want));
        Some((arrived, answer.get("blind")?.as_bool()?))
    }

    /// The name the page's key recorder gives a key, for the keys whose arrival can be checked:
    /// printable ASCII and the named keys. Other text arrives as input events with other names.
    fn key_name(key: &TypedKey) -> Option<String> {
        match key {
            TypedKey::Return => Some("<Enter>".into()),
            TypedKey::Tab => Some("<Tab>".into()),
            TypedKey::Escape => Some("<Escape>".into()),
            TypedKey::Backspace => Some("<Backspace>".into()),
            TypedKey::Char(c) if c.is_ascii_graphic() || *c == ' ' => Some(c.to_string()),
            TypedKey::Char(_) => None,
        }
    }

    fn reset_keys(webview: &Webview) {
        Self::reset_record(webview, "__e2eKeys");
    }

    /// Empties one of the recorder's lists (`__e2eKeys`, `__e2eText`) in the top document and
    /// in every same-origin iframe
    fn reset_record(webview: &Webview, list: &str) {
        let Ok(page) = webview.page() else { return };
        let expression = format!(
            "(() => {{ const clear = (w) => {{ try {{ w.{list} = [] }} catch (error) {{}} }}; \
             clear(window); for (const f of document.querySelectorAll('iframe')) clear(f.contentWindow); return true }})()"
        );
        let _ = page.blocking(FOCUS_QUERY, |done| {
            page.evaluate(&expression, false, None, done)
        });
    }

    /// The text the engine offered the page since the last reset of `__e2eText`, as its
    /// `beforeinput` events carried it (an event without text is named by its input type), for
    /// the top document and each same-origin iframe (`null` for a frame out of reach). The
    /// recorder listens at the window in the capture phase, so this is what arrived before any
    /// editor code ran: text that is here and not in the document was dropped by the editor,
    /// text that is not here never came out of the key event.
    fn inserted(webview: &Webview) -> Option<Value> {
        let page = webview.page().ok()?;
        let expression = "(() => { const text = (w) => { try { return (w.__e2eText || []).join('') } catch (error) { return null } }; \
             return { top: text(window), frames: [...document.querySelectorAll('iframe')].map((f) => text(f.contentWindow)) } })()";
        page.blocking(FOCUS_QUERY, |done| {
            page.evaluate(expression, false, None, done)
        })
        .ok()
    }

    /// A native left click at a point of an element of the top document. The page reports the
    /// element's rectangle (scrolled into view) in CSS pixels; the click is made in the window's
    /// pixels, through the OS's own input, so a cross-origin frame (draw.io) or a canvas gets it
    /// as a user's click. The value carries the element the page finds at that point.
    fn click(&self, selector: &str, fx: f64, fy: f64, timeout: Duration) -> StepResult {
        let target = self.target()?;
        let focus = Self::focus_page(&target.webview)?;
        let page = target.webview.page().map_err(failed)?;
        let quoted = serde_json::to_string(selector).map_err(failed)?;
        let expression = format!(
            "(() => {{ const e = document.querySelector({quoted}); if (!e) return null; \
             e.scrollIntoView({{ block: 'center', inline: 'center' }}); \
             const r = e.getBoundingClientRect(); if (r.width === 0 || r.height === 0) return null; \
             const x = r.left + r.width * {fx}; const y = r.top + r.height * {fy}; \
             const hit = document.elementFromPoint(x, y); \
             return {{ x, y, dpr: window.devicePixelRatio, \
             hit: hit ? hit.tagName + (hit.className ? '.' + String(hit.className).split(' ')[0] : '') : null }} }})()"
        );
        let end = Instant::now() + timeout;
        let spot = loop {
            let answer = page
                .blocking(FOCUS_QUERY, |done| {
                    page.evaluate(&expression, false, None, done)
                })
                .ok();
            if let Some(spot) = answer.filter(|value| value.is_object()) {
                break spot;
            }
            if Instant::now() >= end {
                return Err(failed(format!(
                    "no element matching {selector:?} to click within {} ms",
                    timeout.as_millis()
                )));
            }
            sleep(POLL);
        };
        let number = |key: &str| spot.get(key).and_then(Value::as_f64);
        let (Some(x), Some(y), Some(dpr)) = (number("x"), number("y"), number("dpr")) else {
            return Err(failed(format!(
                "the page's answer for {selector:?} has no point"
            )));
        };
        // CSS pixels of the webview to pixels of the window: the webview's own offset in the
        // window, then the page's pixel ratio (which includes the shell's interface scale)
        let origin = target.webview.position().map_err(failed)?;
        let (px, py) = (f64::from(origin.x) + x * dpr, f64::from(origin.y) + y * dpr);
        input::click(&self.context.ui, &target.window, px, py).map_err(unrunnable)?;
        Self::flush_input(&target.webview);
        Ok(Some(json!({
            "selector": selector,
            "css": { "x": x, "y": y },
            "window": { "x": px, "y": py },
            "hit": spot.get("hit"),
            "focusBefore": focus,
            "focusAfter": Self::focus_state(&target.webview),
        })))
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

    /// A `prep-` query only moves the view (focus, a tab click, a goto), so running it again is
    /// harmless; the page it targets may not have built that part yet when `waitReady` returns.
    /// It runs until it answers with something other than `false`, `null`, `0` or an empty
    /// string, or until its time is up; the last answer is the value either way.
    fn query_prep(&self, js: &str, timeout: Duration) -> StepResult {
        let end = Instant::now() + timeout;
        loop {
            let value = self.query(js, timeout)?;
            let done = match &value {
                None | Some(Value::Null | Value::Bool(false)) => false,
                Some(Value::Number(number)) => number.as_f64() != Some(0.0),
                Some(Value::String(text)) => !text.is_empty(),
                Some(_) => true,
            };
            if done || Instant::now() + PREP_RETRY >= end {
                return Ok(value);
            }
            sleep(PREP_RETRY);
        }
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

    /// The PDF of a Save As the shell owns: the editor's own Save As asks where, the dialog
    /// takes `path`, and the step waits for that file, as `exportPdf` does. The value names the
    /// view that was asked, so a checker can tell its calls from another tab's.
    fn save_as_owned(&self, path: &Path, timeout: Duration) -> StepResult {
        // a leftover file would pass for the copy
        let _ = fs::remove_file(path);
        let ticket = dialog::push(Answer::Path(path.to_path_buf()));
        let asked = match on_core_within(CORE_STALL, crate::host::menu::e2e_save_as_owned) {
            Ok(Some(asked)) => asked.map_err(failed),
            Ok(None) => Err(failed("the shell's core is not up")),
            Err(_) => Err(unrunnable(format!(
                "the core thread did not answer within {CORE_STALL:?}"
            ))),
        };
        let outcome = asked.and_then(|view| {
            wait_for_pdf(path, timeout)
                .map(|bytes| (view, bytes))
                .map_err(failed)
        });
        // a save that never asked for a path must not leave its answer for a later dialog
        dialog::discard(ticket);
        let (view, bytes) = outcome?;
        Ok(Some(json!({
            "path": path.display().to_string(),
            "bytes": bytes,
            "view": view.0,
        })))
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
