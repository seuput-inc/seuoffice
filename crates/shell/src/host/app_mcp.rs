//! Built-in app MCP server, driven off the core thread.
//!
//! The protocol service is sans-I/O and owns non-Send session state. A single
//! dedicated thread owns it; renderer controls are dispatched to the core
//! thread and settle back onto that owner. PDF, HWP and CLI tools therefore do
//! not block the UI or the core thread.

mod agent;
mod transport;

pub(crate) use agent::{AgentMcpEnv, has_hub, start_agent_relays, start_agent_server};

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::TimeZone;
use jscompat::{JsObject, JsonValue};
use main_core::packages::electron_utils::default_save_dir::configured_default_save_dir;
use main_core::packages::mcp_sdk::server::{Command, Ticket};
use main_core::shell::app_settings::read_app_settings;
use main_core::shell::mcp::app_mcp::{
    McpResolved, McpRuntime, McpRuntimeDeps, McpRuntimeHost, McpStatus as CoreStatus, McpStep, OpId,
};
use main_core::shell::mcp::mcp_logger::LogClock;
use main_core::shell::mcp::mcp_server::{McpHost, Settled};
use main_core::shell::mcp::tools::hwp_tools::HwpTextReader;
use shell_contract::runtime::{AppPaths, PathName, Services};
use shell_contract::{Hosts, OsShellHost};
use shell_ipc_d2::hwp::HwpEngineHost;
use shell_ipc_w::SharedSaveDirState;
use shell_ipc_w::home_settings::{DEFAULT_MCP_PORT, McpSettings, McpStatus};
use shell_ipc_w::tabs::TabsState;

use crate::host::HostContext;
use crate::host::w::{on_core, post_core};
use crate::paths::ShellPaths;
use transport::{Event, Transport};

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

struct Clock;
impl McpHost for Clock {
    fn random_uuid(&self) -> String {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("OS random source unavailable");
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0],
            b[1],
            b[2],
            b[3],
            b[4],
            b[5],
            b[6],
            b[7],
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        )
    }
    fn now_ms(&self) -> f64 {
        now_ms()
    }
}
impl LogClock for Clock {
    fn now_ms(&self) -> f64 {
        now_ms()
    }
    fn utc_offset_ms(&self, at_ms: f64) -> f64 {
        chrono::Local
            .timestamp_millis_opt(at_ms as i64)
            .single()
            .map_or(0.0, |date| {
                f64::from(date.offset().local_minus_utc()) * 1000.0
            })
    }
}
impl McpRuntimeHost for Clock {
    fn new_service_host(&self) -> Box<dyn McpHost> {
        Box::new(Clock)
    }
    fn new_log_clock(&self) -> Box<dyn LogClock> {
        Box::new(Clock)
    }
}
fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

struct HwpReader(HwpEngineHost);
impl HwpTextReader for HwpReader {
    fn hwp_to_text(&self, bytes: &[u8]) -> Result<String, String> {
        self.0.hwp_to_text(bytes)
    }
}

fn settings_value(settings: McpSettings) -> JsonValue {
    let mut value = JsObject::new();
    value.set("enabled", JsonValue::Bool(settings.enabled));
    value.set("port", JsonValue::Number(settings.port as f64));
    value.set("background", JsonValue::Bool(settings.background));
    value.set("logging", JsonValue::Bool(settings.logging));
    JsonValue::Object(value)
}
fn load_settings(path: &str) -> McpSettings {
    let settings = read_app_settings(path);
    let flag = |key| matches!(settings.get(key), Some(JsonValue::Bool(true)));
    let port = match settings.get("mcpPort") {
        Some(JsonValue::Number(n))
            if n.is_finite() && n.fract() == 0.0 && *n > 0.0 && *n < 65536.0 =>
        {
            *n as u32
        }
        _ => DEFAULT_MCP_PORT,
    };
    McpSettings {
        enabled: flag("mcpEnabled"),
        port,
        background: flag("mcpBackground"),
        logging: flag("mcpLogging"),
    }
}
fn status_of(status: CoreStatus) -> McpStatus {
    McpStatus {
        running: status.running,
        settings: McpSettings {
            enabled: status.enabled,
            port: status.port as u32,
            background: status.background,
            logging: status.logging,
        },
        url: status.url,
        capabilities: status.capabilities,
    }
}
struct Mirror {
    status: McpStatus,
    logs: Vec<String>,
    clear_pending: usize,
}

enum Lifecycle {
    Idle,
    Running(mpsc::Sender<Message>),
    Retryable(mpsc::Sender<Message>),
    Stopping(mpsc::Sender<Message>, Arc<StopCompletion>),
    Stopped,
    Failed(String),
}

#[derive(Default)]
struct StopCompletion {
    result: Mutex<Option<Result<(), String>>>,
    ready: Condvar,
}

impl StopCompletion {
    fn finish(&self, result: Result<(), String>) {
        *lock(&self.result) = Some(result);
        self.ready.notify_all();
    }

    fn wait(&self) -> Result<(), String> {
        let mut result = lock(&self.result);
        while result.is_none() {
            result = self
                .ready
                .wait(result)
                .unwrap_or_else(|error| error.into_inner());
        }
        result.as_ref().expect("completion was signalled").clone()
    }
}

fn finish_stop(state: &Mutex<Lifecycle>, result: Result<(), String>, terminal: bool) {
    let completion = {
        let mut state = lock(state);
        let previous = std::mem::replace(&mut *state, Lifecycle::Stopped);
        match previous {
            Lifecycle::Stopping(tx, completion) => {
                *state = match &result {
                    Ok(()) => Lifecycle::Stopped,
                    Err(error) if terminal => Lifecycle::Failed(error.clone()),
                    Err(_) => Lifecycle::Retryable(tx),
                };
                Some(completion)
            }
            Lifecycle::Running(_) | Lifecycle::Retryable(_) if terminal => {
                *state = Lifecycle::Failed(
                    result
                        .as_ref()
                        .err()
                        .cloned()
                        .unwrap_or_else(|| "MCP actor stopped unexpectedly".into()),
                );
                None
            }
            other => {
                *state = other;
                None
            }
        }
    };
    if let Some(completion) = completion {
        completion.finish(result);
    }
}

fn stop_lifecycle(lifecycle: &Mutex<Lifecycle>) -> Result<(), String> {
    let completion = {
        let mut state = lock(lifecycle);
        match &*state {
            Lifecycle::Idle => {
                *state = Lifecycle::Stopped;
                return Ok(());
            }
            Lifecycle::Stopped => return Ok(()),
            Lifecycle::Failed(error) => return Err(error.clone()),
            Lifecycle::Stopping(_, completion) => completion.clone(),
            Lifecycle::Running(tx) | Lifecycle::Retryable(tx) => {
                let tx = tx.clone();
                if tx.send(Message::Shutdown).is_err() {
                    let error = "MCP runtime stopped before shutdown was acknowledged".to_owned();
                    *state = Lifecycle::Failed(error.clone());
                    return Err(error);
                }
                let completion = Arc::new(StopCompletion::default());
                *state = Lifecycle::Stopping(tx, completion.clone());
                completion
            }
        }
    };
    completion.wait()
}

enum Message {
    Event(Event),
    Apply(McpSettings, mpsc::Sender<Result<McpStatus, String>>),
    ToolDone(String, Ticket, Settled),
    ToolDropped(String, Ticket),
    ClearLogs,
    Reveal,
    Shutdown,
}

struct ToolCompletion {
    session: Option<String>,
    ticket: Ticket,
    tx: mpsc::Sender<Message>,
}

impl ToolCompletion {
    fn finish(mut self, answer: Settled) {
        if let Some(session) = self.session.take() {
            let _ = self
                .tx
                .send(Message::ToolDone(session, self.ticket, answer));
        }
    }
}

impl Drop for ToolCompletion {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = self.tx.send(Message::ToolDropped(session, self.ticket));
        }
    }
}

/// One process-wide host, installed before HomeApp and started after batches
/// register their controls. Home queries use a mirror, never the busy actor.
pub(crate) struct AppMcp {
    lifecycle: Arc<Mutex<Lifecycle>>,
    mirror: Arc<Mutex<Mirror>>,
    settings: McpSettings,
    save_dir: SharedSaveDirState,
    resources: Option<PathBuf>,
    log_file: PathBuf,
    os: OsShellHost,
    version: String,
}
impl AppMcp {
    pub(crate) fn new(cx: &HostContext, hosts: &Hosts) -> Self {
        let paths = ShellPaths::new();
        let user_data = paths.path(PathName::UserData);
        let settings = load_settings(&user_data.join("app-settings.json").to_string_lossy());
        let status = McpStatus {
            running: false,
            settings,
            url: None,
            capabilities: vec![
                "docs".into(),
                "slides".into(),
                "sheets".into(),
                "pdf".into(),
                "hwp".into(),
            ],
        };
        Self {
            lifecycle: Arc::new(Mutex::new(Lifecycle::Idle)),
            mirror: Arc::new(Mutex::new(Mirror {
                status,
                logs: Vec::new(),
                clear_pending: 0,
            })),
            settings,
            save_dir: hosts
                .get::<SharedSaveDirState>()
                .cloned()
                .expect("shared save directory state"),
            resources: crate::paths::resources_path(&cx.app),
            log_file: user_data.join("mcp-log.txt"),
            os: hosts.get::<OsShellHost>().cloned().expect("OS shell host"),
            version: cx.app.package_info().version.to_string(),
        }
    }

    /// Called after A/W/D2 registration, once per process.
    pub(crate) fn start(&self, services: &Services) -> Result<(), String> {
        let mut slot = lock(&self.lifecycle);
        match &*slot {
            Lifecycle::Idle => {}
            Lifecycle::Running(_) => return Ok(()),
            Lifecycle::Retryable(_) | Lifecycle::Stopping(..) | Lifecycle::Stopped => {
                return Err("MCP runtime is stopping or stopped".into());
            }
            Lifecycle::Failed(error) => return Err(error.clone()),
        }
        let hwp = services
            .hosts
            .get::<HwpEngineHost>()
            .cloned()
            .ok_or("HWP engine unavailable")?;
        let (tx, rx) = mpsc::channel();
        let actor_tx = tx.clone();
        let mirror = self.mirror.clone();
        let save_dir = self.save_dir.clone();
        let resources = self.resources.clone();
        let log_file = self.log_file.clone();
        let os = self.os.clone();
        let version = self.version.clone();
        let settings = self.settings;
        let lifecycle = self.lifecycle.clone();
        std::thread::Builder::new()
            .name("app-mcp".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> Result<(), String> {
                        let sink_tx = actor_tx.clone();
                        let transport = Transport::new(Arc::new(move |event| {
                            let _ = sink_tx.send(Message::Event(event));
                        }));
                        let mut runtime = McpRuntime::new(Box::new(Clock));
                        let cli = resources.map(|path| {
                            Rc::new(crate::host::cli::BundledCli::new(path))
                                as Rc<dyn main_core::shell::mcp::cli_runner::CliRunner>
                        });
                        runtime.configure_mcp_runtime(McpRuntimeDeps {
                            version,
                            default_save_dir: Rc::new(move || {
                                configured_default_save_dir(
                                    &ShellPaths::new(),
                                    &mut lock(&save_dir),
                                    now_ms(),
                                )
                                .unwrap_or_else(|_| {
                                    ShellPaths::new()
                                        .path(PathName::Documents)
                                        .to_string_lossy()
                                        .into_owned()
                                })
                            }),
                            docs_control: true,
                            slides_control: true,
                            sheets_control: true,
                            cli_runner: cli,
                            open_documents_control: true,
                            resolve_target: true,
                            diagram_control: true,
                            diagram_is_detached: Some(Rc::new(|id| {
                                let id = id.to_owned();
                                on_core(move |provided| {
                                    provided
                                        .all::<TabsState>()
                                        .into_iter()
                                        .next()
                                        .is_some_and(|tabs| tabs.is_detached_tab(&id))
                                })
                                .unwrap_or(false)
                            })),
                            log_file_path: Some(log_file.to_string_lossy().into_owned()),
                            pdf_reader: Rc::new(crate::host::d2::pdf::text_reader::PdfTextReader),
                            hwp_reader: Rc::new(HwpReader(hwp)),
                        });
                        let mut actor = Actor {
                            runtime,
                            transport,
                            mirror,
                            tx: actor_tx,
                            pending: HashMap::new(),
                            os,
                            shutdown_op: None,
                            shutdown_result: None,
                            tools_in_flight: 0,
                            tool_shutdown_failure: None,
                            quiescing: false,
                        };
                        let value = settings_value(settings);
                        let (_, step) = actor.runtime.start_mcp_from_settings(Some(&value));
                        actor.step(step);
                        while let Ok(message) = rx.recv() {
                            if actor.message(message) {
                                let result = actor.shutdown_result.take().expect("shutdown result");
                                actor.shutdown_op = None;
                                if let Err(error) = result {
                                    finish_stop(&lifecycle, Err(error), false);
                                    continue;
                                }
                                if let Some(error) = actor.tool_shutdown_failure.take() {
                                    finish_stop(&lifecycle, Err(error), false);
                                    continue;
                                }
                                actor.transport.shutdown_and_wait()?;
                                actor.quiescing = false;
                                actor.step(McpStep::default());
                                drop(actor);
                                return Ok(());
                            }
                        }
                        Err("MCP actor stopped unexpectedly".into())
                    },
                ));
                let result = match outcome {
                    Ok(result) => result,
                    Err(_) => Err("MCP actor panicked during execution".into()),
                };
                finish_stop(&lifecycle, result, true);
            })
            .map_err(|error| error.to_string())?;
        *slot = Lifecycle::Running(tx);
        Ok(())
    }
    pub(crate) fn status(&self) -> McpStatus {
        lock(&self.mirror).status.clone()
    }
    pub(crate) fn logs(&self) -> Vec<String> {
        lock(&self.mirror).logs.clone()
    }
    pub(crate) fn log_file(&self) -> Option<PathBuf> {
        Some(self.log_file.clone())
    }
    pub(crate) fn clear_logs(&self) {
        if let Lifecycle::Running(tx) = &*lock(&self.lifecycle) {
            let mut mirror = lock(&self.mirror);
            if tx.send(Message::ClearLogs).is_ok() {
                mirror.logs.clear();
                mirror.clear_pending += 1;
            }
        }
    }
    pub(crate) fn reveal_log_file(&self) {
        if let Lifecycle::Running(tx) = &*lock(&self.lifecycle) {
            let _ = tx.send(Message::Reveal);
        }
    }
    pub(crate) fn apply(&self, settings: McpSettings) -> Result<McpStatus, String> {
        let (reply, answer) = mpsc::channel();
        {
            let state = lock(&self.lifecycle);
            let Lifecycle::Running(tx) = &*state else {
                return Err("MCP runtime is not running".into());
            };
            tx.send(Message::Apply(settings, reply))
                .map_err(|_| "MCP runtime stopped".to_owned())?;
        }
        answer
            .recv()
            .map_err(|_| "MCP runtime stopped".to_owned())?
    }
    pub(crate) fn stop(&self) -> Result<(), String> {
        stop_lifecycle(&self.lifecycle)
    }
}

struct Actor {
    runtime: McpRuntime<'static>,
    transport: Transport,
    mirror: Arc<Mutex<Mirror>>,
    tx: mpsc::Sender<Message>,
    pending: HashMap<u32, mpsc::Sender<Result<McpStatus, String>>>,
    os: OsShellHost,
    shutdown_op: Option<u32>,
    shutdown_result: Option<Result<(), String>>,
    tools_in_flight: usize,
    tool_shutdown_failure: Option<String>,
    quiescing: bool,
}
impl Actor {
    fn step(&mut self, step: McpStep) {
        for command in step.commands {
            if let Err(command) = self.transport.execute(command) {
                match command {
                    Command::ToolCall {
                        session_id,
                        ticket,
                        call,
                    } => {
                        self.tools_in_flight += 1;
                        let completion = ToolCompletion {
                            session: Some(session_id),
                            ticket,
                            tx: self.tx.clone(),
                        };
                        post_core(move |provided, services| {
                            crate::host::app_mcp_dispatch::dispatch(
                                provided,
                                services,
                                call,
                                Box::new(move |answer| {
                                    completion.finish(answer);
                                }),
                            );
                        });
                    }
                    Command::Aborted(_) => {} // The SDK lets already-started tools finish.
                    other => eprintln!("shell: unexpected MCP command: {other:?}"),
                }
            }
        }
        let status = status_of(self.runtime.mcp_status());
        let logs = self.runtime.get_mcp_recent_logs();
        {
            let mut mirror = lock(&self.mirror);
            if !self.quiescing {
                mirror.status = status;
            }
            if mirror.clear_pending == 0 {
                mirror.logs = logs;
            }
        }
        for (OpId(id), result) in step.settled {
            if self.shutdown_op == Some(id) {
                self.shutdown_result = Some(result.as_ref().map(|_| ()).map_err(Clone::clone));
            }
            if let Some(reply) = self.pending.remove(&id) {
                let answer = match result {
                    Ok(McpResolved::Status(status)) => Ok(status_of(status)),
                    Ok(McpResolved::Undefined) => Err("MCP operation returned no status".into()),
                    Err(error) => Err(error),
                };
                let _ = reply.send(answer);
            } else if let Err(error) = result {
                eprintln!("shell: MCP: {error}");
            }
        }
    }
    fn message(&mut self, message: Message) -> bool {
        match message {
            Message::Apply(settings, reply) => {
                let value = settings_value(settings);
                let (OpId(id), step) = self.runtime.apply_mcp_settings(Some(&value));
                self.pending.insert(id, reply);
                self.step(step);
            }
            Message::Event(event) => {
                if self.quiescing && matches!(&event, Event::Request(..) | Event::Connected(_)) {
                    return self.shutdown_ready();
                }
                let commands = match self.runtime.service_mut() {
                    Some(service) => match event {
                        Event::Listening(id, port) => service.on_listening(id, f64::from(port)),
                        Event::ListenError(id, code, text) => {
                            service.on_listen_error(id, code, &text)
                        }
                        Event::ListenerClosed(id) => service.on_listener_closed(id),
                        Event::Connected(id) => {
                            service.on_connection(id);
                            Vec::new()
                        }
                        Event::SocketClosed(id) => {
                            service.on_socket_closed(id);
                            Vec::new()
                        }
                        Event::Request(id, head, body) => service.on_request(id, &head, &body),
                        Event::ResponseClosed(id) => service.on_response_closed(id),
                        Event::Timer(id, generation, repeating) => {
                            if self.transport.accept_timer(id, generation, repeating) {
                                service.on_timer(id)
                            } else {
                                Vec::new()
                            }
                        }
                    },
                    None => Vec::new(),
                };
                let step = self.runtime.route(commands);
                self.step(step);
            }
            Message::ToolDone(session, ticket, answer) => {
                self.tools_in_flight -= 1;
                let commands = self.runtime.service_mut().map_or_else(Vec::new, |service| {
                    service.resume_tool(&session, ticket, answer)
                });
                let step = self.runtime.route(commands);
                self.step(step);
            }
            Message::ToolDropped(session, ticket) => {
                self.tools_in_flight -= 1;
                let error = "MCP tool dispatch or completion was dropped".to_owned();
                if self.quiescing {
                    self.tool_shutdown_failure = Some(error.clone());
                }
                let commands = self.runtime.service_mut().map_or_else(Vec::new, |service| {
                    service.resume_tool(&session, ticket, Err(error))
                });
                let step = self.runtime.route(commands);
                self.step(step);
            }
            Message::ClearLogs => {
                self.runtime.clear_mcp_logs();
                let mut mirror = lock(&self.mirror);
                mirror.clear_pending = mirror.clear_pending.saturating_sub(1);
                drop(mirror);
                self.step(McpStep::default());
            }
            Message::Reveal => {
                self.runtime.reveal_mcp_log_file();
                if let Some(path) = self.runtime.mcp_log_file_path() {
                    self.os.show_item_in_folder(path.as_ref());
                }
            }
            Message::Shutdown => {
                self.quiescing = true;
                let (OpId(id), step) = self.runtime.stop_mcp();
                self.shutdown_op = Some(id);
                self.step(step);
            }
        }
        self.shutdown_ready()
    }

    fn shutdown_ready(&self) -> bool {
        match self.shutdown_result.as_ref() {
            Some(Err(_)) => true,
            Some(Ok(())) => self.tools_in_flight == 0,
            None => false,
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[test]
    fn concurrent_stops_wait_for_completion_and_share_the_result() {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(Lifecycle::Running(tx.clone())));
        assert!(tx.send(Message::ClearLogs).is_ok());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let actor_state = state.clone();
        let actor = std::thread::spawn(move || {
            assert!(matches!(rx.recv().unwrap(), Message::ClearLogs));
            assert!(matches!(rx.recv().unwrap(), Message::Shutdown));
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            finish_stop(&actor_state, Ok(()), true);
        });
        let (done_tx, done_rx) = mpsc::channel();
        let first_state = state.clone();
        let first_tx = done_tx.clone();
        let first =
            std::thread::spawn(move || first_tx.send(stop_lifecycle(&first_state)).unwrap());
        started_rx.recv().unwrap();
        let second_state = state.clone();
        let second =
            std::thread::spawn(move || done_tx.send(stop_lifecycle(&second_state)).unwrap());
        assert!(done_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        assert_eq!(done_rx.recv().unwrap(), Ok(()));
        assert_eq!(done_rx.recv().unwrap(), Ok(()));
        first.join().unwrap();
        second.join().unwrap();
        actor.join().unwrap();
        assert_eq!(stop_lifecycle(&state), Ok(()));
    }

    #[test]
    fn failed_stop_is_reported_and_can_be_retried() {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(Lifecycle::Running(tx)));
        let actor_state = state.clone();
        let actor = std::thread::spawn(move || {
            assert!(matches!(rx.recv().unwrap(), Message::Shutdown));
            finish_stop(&actor_state, Err("stop failed".into()), false);
            assert!(matches!(rx.recv().unwrap(), Message::Shutdown));
            finish_stop(&actor_state, Ok(()), true);
        });
        assert_eq!(stop_lifecycle(&state), Err("stop failed".into()));
        assert_eq!(stop_lifecycle(&state), Ok(()));
        actor.join().unwrap();
    }

    #[test]
    fn unstarted_stop_is_final_and_disconnected_actor_is_not_success() {
        let idle = Mutex::new(Lifecycle::Idle);
        assert_eq!(stop_lifecycle(&idle), Ok(()));
        assert!(matches!(*lock(&idle), Lifecycle::Stopped));
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let broken = Mutex::new(Lifecycle::Running(tx));
        assert!(stop_lifecycle(&broken).is_err());
        assert!(stop_lifecycle(&broken).is_err());
    }
}

#[cfg(test)]
mod tool_completion_tests {
    use super::*;
    struct TestOs;
    impl shell_contract::hosts::OsShell for TestOs {
        fn open_external(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn open_path(&self, _: &std::path::Path) -> String {
            String::new()
        }
        fn show_item_in_folder(&self, _: &std::path::Path) {}
        fn trash_item(&self, _: &std::path::Path) -> Result<(), String> {
            Ok(())
        }
        fn trash_item_authorized(
            &self,
            path: &std::path::Path,
            admit: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
        ) -> Result<(), String> {
            admit(&mut || self.trash_item(path))
        }
    }

    #[test]
    fn settled_stop_waits_for_late_tool_completion() {
        let (tx, _rx) = mpsc::channel();
        let runtime = McpRuntime::new(Box::new(Clock));
        let mirror = Arc::new(Mutex::new(Mirror {
            status: status_of(runtime.mcp_status()),
            logs: Vec::new(),
            clear_pending: 0,
        }));
        lock(&mirror).status.running = true;
        let mut actor = Actor {
            runtime,
            transport: Transport::new(Arc::new(|_| {})),
            mirror: mirror.clone(),
            tx,
            pending: HashMap::new(),
            os: Arc::new(TestOs),
            shutdown_op: None,
            shutdown_result: None,
            tools_in_flight: 1,
            tool_shutdown_failure: None,
            quiescing: false,
        };
        assert!(!actor.message(Message::Shutdown));
        assert!(lock(&mirror).status.running);
        assert!(actor.message(Message::ToolDone("session".into(), Ticket(1), Ok(None))));
        assert!(lock(&mirror).status.running);
        actor.transport.shutdown_and_wait().unwrap();
        actor.quiescing = false;
        actor.step(McpStep::default());
        assert!(!lock(&mirror).status.running);
    }

    #[test]
    fn dropped_dispatch_reports_failure_to_actor() {
        let (tx, rx) = mpsc::channel();
        drop(ToolCompletion {
            session: Some("session".into()),
            ticket: Ticket(7),
            tx,
        });
        assert!(
            matches!(rx.recv().unwrap(), Message::ToolDropped(session, Ticket(7)) if session == "session")
        );
    }

    #[test]
    fn completed_dispatch_emits_only_one_settlement() {
        let (tx, rx) = mpsc::channel();
        ToolCompletion {
            session: Some("session".into()),
            ticket: Ticket(8),
            tx,
        }
        .finish(Ok(None));
        assert!(
            matches!(rx.recv().unwrap(), Message::ToolDone(session, Ticket(8), Ok(None)) if session == "session")
        );
        assert!(rx.try_recv().is_err());
    }
}
