//! The MCP servers of one agent session (`apps/shell/src/main/agent/agent-mcp.ts`
//! and `agent-mcp-relay.ts`): the app's own server for open documents, and a relay
//! per server of the person's MCP hub.
//!
//! Each is the sans-I/O service of main-core (`start_agent_mcp_server`,
//! `AgentMcpRelays`) driven by one thread of its own over the same loopback
//! [`Transport`] the app's MCP server uses: loopback, a port the system picks,
//! a fresh token of the session. A tool of the app's server that needs an
//! editor is dispatched to the core thread like the app MCP's tools
//! (`app_mcp_dispatch::dispatch`); the two skill tools reach the agent window
//! through the session's [`SkillHooks`]; a relay's calls reach the hub (core
//! thread) and its `ask` tools the agent window's card.
//!
//! Both are started from a worker (`AgentServices::start_app_mcp`,
//! `start_relays`) and answer when the services listen. Stopping is a message
//! to the thread and returns at once.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, mpsc};

use jscompat::JsonValue;
use main_core::packages::electron_utils::default_save_dir::configured_default_save_dir;
use main_core::packages::mcp_sdk::server::{Command, StartId, StopId, Ticket};
use main_core::shell::agent::agent_mcp::{
    AgentMcpDeps, AgentMcpHost, AgentMcpServer, SourceRedactor, start_agent_mcp_server,
};
use main_core::shell::agent::agent_mcp_relay::{
    AgentMcpRelays, AgentMcpRelaysDeps, AgentRelayAnswer, AgentRelayAsk, AgentRelayAskRequest,
    AgentRelayHub, AgentRelayScope, RelayCallId, RelayDone, RelayStep,
};
use main_core::shell::agent::agent_sessions::{AgentMcpRelay, RelayAnswer, RelayAskRequest};
use main_core::shell::agent::agent_skills::{AgentOpenTab, AgentSkillProposalInput};
use main_core::shell::mcp::mcp_server::{HostCall, McpHost, McpServerService, Settled};
use main_core::shell::mcp::tools::open_documents_tools::list_call;
use shell_contract::Hosts;
use shell_contract::runtime::{AppPaths, PathName};
use shell_ipc_a::agent::{RelayAsker, RelaysStarted, SkillHooks};
use shell_ipc_a::mcp::McpHubPort;
use shell_ipc_d2::hwp::HwpEngineHost;
use shell_ipc_w::SharedSaveDirState;

use super::transport::{Event, Transport};
use super::{Clock, HwpReader, lock, now_ms};
use crate::host::HostContext;
use crate::host::w::{on_core, post_core, post_core_checked};
use crate::paths::ShellPaths;

/// What a session's servers are made from: what the shell's own app MCP server
/// is given, captured once at install
pub(crate) struct AgentMcpEnv {
    version: String,
    save_dir: SharedSaveDirState,
    resources: Option<PathBuf>,
    hwp: HwpEngineHost,
}

impl AgentMcpEnv {
    /// `None` where the shell has no HWP engine: the app has no editors to offer a session
    pub(crate) fn new(cx: &HostContext, hosts: &Hosts) -> Option<Self> {
        Some(Self {
            version: cx.app.package_info().version.to_string(),
            save_dir: hosts.get::<SharedSaveDirState>().cloned()?,
            resources: crate::paths::resources_path(&cx.app),
            hwp: hosts.get::<HwpEngineHost>().cloned()?,
        })
    }
}

/// `crypto` for the session's services
struct AgentHost;

impl AgentMcpHost for AgentHost {
    fn random_bytes(&self, n: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; n];
        getrandom::fill(&mut bytes).expect("OS random source unavailable");
        bytes
    }

    fn random_uuid(&self) -> String {
        McpHost::random_uuid(&Clock)
    }

    fn now_ms(&self) -> f64 {
        now_ms()
    }
}

/// What a withheld document reads as when the source could not be redacted: never the source
const UNREDACTABLE: &str =
    "[This document could not be read safely: its withheld words could not be hidden.]";

/// `redactMarkdownSource` and `redactHtmlSource` of `@seuoffice/file-parse`: the source
/// with its withheld words as their labels
struct Redactor;

impl SourceRedactor for Redactor {
    fn redact_markdown_source(&self, source: &str) -> String {
        file_parse::markdown::redact_markdown_source(source).unwrap_or_else(|error| {
            eprintln!("shell: agent MCP: redacting Markdown: {error}");
            UNREDACTABLE.to_owned()
        })
    }

    fn redact_html_source(&self, source: &str) -> String {
        file_parse::html::redact_html_source(source).unwrap_or_else(|error| {
            eprintln!("shell: agent MCP: redacting HTML: {error}");
            UNREDACTABLE.to_owned()
        })
    }
}

/// Delivers a transport event to the service it belongs to
fn route(service: &mut McpServerService<'_>, transport: &Transport, event: Event) -> Vec<Command> {
    match event {
        Event::Listening(id, port) => service.on_listening(id, f64::from(port)),
        Event::ListenError(id, code, text) => service.on_listen_error(id, code, &text),
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
            if transport.accept_timer(id, generation, repeating) {
                service.on_timer(id)
            } else {
                Vec::new()
            }
        }
    }
}

// ---- the app's own server of a session ----

/// A started server: what the program is given, and the way to stop it
pub(crate) struct ServerStarted {
    pub url: String,
    pub token: String,
    /// returns at once: the thread winds the server down
    pub stop: Box<dyn FnOnce() + Send>,
}

enum ServerMsg {
    Event(Event),
    ToolDone(String, Ticket, Settled),
    ToolDropped(String, Ticket),
    Stop,
}

/// A tool call handed to the core thread: its answer, or its being dropped, comes back as a message
struct ToolCompletion {
    session: Option<String>,
    ticket: Ticket,
    tx: mpsc::Sender<ServerMsg>,
}

impl ToolCompletion {
    fn finish(mut self, answer: Settled) {
        if let Some(session) = self.session.take() {
            let _ = self
                .tx
                .send(ServerMsg::ToolDone(session, self.ticket, answer));
        }
    }
}

impl Drop for ToolCompletion {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = self.tx.send(ServerMsg::ToolDropped(session, self.ticket));
        }
    }
}

/// `AgentSkillProposalInput` from the JSON the `propose_skill` tool handed over
fn proposal_of(value: Option<&JsonValue>) -> Result<AgentSkillProposalInput, String> {
    let value = value.ok_or("missing proposal")?;
    let word = |key: &str| match value.get(key).as_deref() {
        Some(JsonValue::String(text)) => text.clone(),
        _ => String::new(),
    };
    Ok(AgentSkillProposalInput {
        title: word("title"),
        name: word("name"),
        description: word("description"),
        body: word("body"),
        wants_template: matches!(
            value.get("wantsTemplate").as_deref(),
            Some(JsonValue::Bool(true))
        ),
        target_id: word("targetId"),
        suggested_template_path: word("suggestedTemplatePath"),
    })
}

/// `await context.openTabs()`: the documents open in tabs now; none where it rejected
fn open_tabs_of(settled: Settled) -> Vec<AgentOpenTab> {
    let Ok(Some(JsonValue::Array(tabs))) = settled else {
        return Vec::new();
    };
    tabs.iter()
        .map(|tab| AgentOpenTab {
            title: match tab.get("title").as_deref() {
                Some(JsonValue::String(title)) => title.clone(),
                _ => String::new(),
            },
            file_path: match tab.get("filePath").as_deref() {
                Some(JsonValue::String(path)) => Some(path.clone()),
                _ => None,
            },
            dirty: matches!(tab.get("dirty").as_deref(), Some(JsonValue::Bool(true))),
        })
        .collect()
}

/// A tool call, on the core thread: the two skill tools show a card or a
/// proposal through the session's hooks, every other call is an editor's
fn run_tool(
    provided: &shell_contract::provided::Provided,
    services: &shell_contract::runtime::Services,
    hooks: Arc<dyn SkillHooks>,
    call: HostCall,
    completion: ToolCompletion,
) {
    if call.control != "AgentSkillToolHooks" {
        crate::host::app_mcp_dispatch::dispatch(
            provided,
            services,
            call,
            Box::new(move |answer| completion.finish(answer)),
        );
        return;
    }
    let done = |completion: ToolCompletion| -> Box<dyn FnOnce(Result<(), String>) + Send> {
        Box::new(move |result| completion.finish(result.map(|()| None)))
    };
    match call.method {
        "proposeSkill" => {
            let input = match proposal_of(call.args.first()) {
                Ok(input) => input,
                Err(error) => return completion.finish(Err(error)),
            };
            crate::host::app_mcp_dispatch::dispatch(
                provided,
                services,
                list_call(),
                Box::new(move |listed| {
                    hooks.propose_skill(input, open_tabs_of(listed), done(completion));
                }),
            );
        }
        "proposeInstall" => {
            let address = match call.args.first() {
                Some(JsonValue::String(address)) => address.clone(),
                _ => return completion.finish(Err("missing address".to_owned())),
            };
            hooks.propose_install(address, done(completion));
        }
        other => completion.finish(Err(format!("unknown AgentSkillToolHooks.{other}"))),
    }
}

struct ServerLoop {
    server: AgentMcpServer<'static>,
    transport: Transport,
    tx: mpsc::Sender<ServerMsg>,
    hooks: Arc<dyn SkillHooks>,
    start: StartId,
    ready: Option<mpsc::Sender<Result<(String, String), String>>>,
    stopping: Option<StopId>,
    done: bool,
}

impl ServerLoop {
    fn process(&mut self, commands: Vec<Command>) {
        for command in commands {
            let Err(command) = self.transport.execute(command) else {
                continue;
            };
            match command {
                Command::ToolCall {
                    session_id,
                    ticket,
                    call,
                } => {
                    let completion = ToolCompletion {
                        session: Some(session_id),
                        ticket,
                        tx: self.tx.clone(),
                    };
                    let hooks = self.hooks.clone();
                    post_core(move |provided, services| {
                        run_tool(provided, services, hooks, call, completion);
                    });
                }
                Command::StartSettled { start, outcome } if start == self.start => {
                    if outcome.is_err() {
                        self.done = true;
                    }
                    if let Some(ready) = self.ready.take() {
                        let _ = ready.send(match outcome {
                            Ok(()) => Ok((self.server.url(), self.server.token().to_owned())),
                            Err(error) => Err(error.text),
                        });
                    }
                }
                Command::StopSettled { stop } if Some(stop) == self.stopping => {
                    self.done = true;
                }
                _ => {}
            }
        }
    }

    fn message(&mut self, message: ServerMsg) {
        let commands = match message {
            ServerMsg::Event(event) => route(self.server.service(), &self.transport, event),
            ServerMsg::ToolDone(session, ticket, answer) => {
                self.server.service().resume_tool(&session, ticket, answer)
            }
            ServerMsg::ToolDropped(session, ticket) => self.server.service().resume_tool(
                &session,
                ticket,
                Err("MCP tool dispatch or completion was dropped".to_owned()),
            ),
            ServerMsg::Stop => {
                if self.stopping.is_some() {
                    return;
                }
                let (stop, commands) = self.server.stop();
                self.stopping = Some(stop);
                commands
            }
        };
        self.process(commands);
    }
}

fn run_server(
    env: &AgentMcpEnv,
    project_root: &str,
    hooks: Arc<dyn SkillHooks>,
    tx: mpsc::Sender<ServerMsg>,
    rx: &mpsc::Receiver<ServerMsg>,
    ready: mpsc::Sender<Result<(String, String), String>>,
) {
    let sink = tx.clone();
    let transport = Transport::new(Arc::new(move |event| {
        let _ = sink.send(ServerMsg::Event(event));
    }));
    let save_dir = env.save_dir.clone();
    let cli = env.resources.clone().map(|path| {
        Rc::new(crate::host::cli::BundledCli::new(path))
            as Rc<dyn main_core::shell::mcp::cli_runner::CliRunner>
    });
    let deps = Rc::new(AgentMcpDeps {
        version: env.version.clone(),
        default_save_dir: Rc::new(move || {
            configured_default_save_dir(&ShellPaths::new(), &mut lock(&save_dir), now_ms())
                .unwrap_or_else(|_| {
                    ShellPaths::new()
                        .path(PathName::Documents)
                        .to_string_lossy()
                        .into_owned()
                })
        }),
        docs_control: true,
        slides_control: true,
        // the deck bridge answers `deckWithholds`, so an agent session edits a deck only while it
        // withholds nothing
        slides_deck_withholds: true,
        sheets_control: true,
        open_documents_control: true,
        resolve_target: true,
        skill_tools: true,
        cli,
        hwp_reader: Rc::new(HwpReader(env.hwp.clone())),
        pdf_reader: Rc::new(crate::host::d2::pdf::text_reader::PdfTextReader),
        redactor: Rc::new(Redactor),
    });
    let (server, start, commands) = start_agent_mcp_server(deps, project_root, Rc::new(AgentHost));
    let mut running = ServerLoop {
        server,
        transport,
        tx,
        hooks,
        start,
        ready: Some(ready),
        stopping: None,
        done: false,
    };
    running.process(commands);
    while !running.done {
        let Ok(message) = rx.recv() else {
            break;
        };
        running.message(message);
    }
    if let Err(error) = running.transport.shutdown_and_wait() {
        eprintln!("shell: agent MCP: stopping the transport: {error}");
    }
}

/// `startAgentMcpServer({ ...mcp, skillTools }, projectRoot)`: one session's server, started and
/// waited for here (a worker)
pub(crate) fn start_agent_server(
    env: Arc<AgentMcpEnv>,
    project_root: String,
    hooks: Arc<dyn SkillHooks>,
) -> Result<ServerStarted, String> {
    let (tx, rx) = mpsc::channel();
    let (ready, started) = mpsc::channel();
    let actor_tx = tx.clone();
    let failed = ready.clone();
    std::thread::Builder::new()
        .name("agent-mcp".into())
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                run_server(&env, &project_root, hooks, actor_tx, &rx, ready);
            }));
            if outcome.is_err() {
                let _ = failed.send(Err("agent MCP server panicked".to_owned()));
            }
        })
        .map_err(|error| error.to_string())?;
    match started.recv() {
        Ok(Ok((url, token))) => Ok(ServerStarted {
            url,
            token,
            stop: Box::new(move || {
                let _ = tx.send(ServerMsg::Stop);
            }),
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("agent MCP server stopped before it started".to_owned()),
    }
}

// ---- the relays of the person's servers ----

enum RelayMsg {
    Event(usize, Event),
    HubAnswer(String, Result<Option<JsonValue>, ()>),
    AskAnswer(RelayCallId, AgentRelayAnswer),
    Stop,
}

/// The hub, which lives on the core thread, as the relays use it
struct HubLink {
    tx: mpsc::Sender<RelayMsg>,
}

fn hub_of(provided: &shell_contract::provided::Provided) -> Option<Rc<McpHubPort>> {
    provided.all::<McpHubPort>().into_iter().next()
}

impl AgentRelayHub for HubLink {
    fn editor_list(&self) -> Vec<JsonValue> {
        on_core(|provided| hub_of(provided).map(|hub| hub.relay_editor_list()))
            .flatten()
            .unwrap_or_default()
    }

    fn call(&self, caller: &str, server: &str, tool: &str, input: &JsonValue, call_id: &str) {
        let (caller, server, tool, input, call_id) = (
            caller.to_owned(),
            server.to_owned(),
            tool.to_owned(),
            input.clone(),
            call_id.to_owned(),
        );
        let (answer, lost, id) = (self.tx.clone(), self.tx.clone(), call_id.clone());
        post_core_checked(
            move |provided, _| {
                let Some(hub) = hub_of(provided) else {
                    let _ = answer.send(RelayMsg::HubAnswer(call_id, Err(())));
                    return;
                };
                let id = call_id.clone();
                hub.relay_call(
                    &caller,
                    &server,
                    &tool,
                    &input,
                    &call_id,
                    Box::new(move |settled| {
                        let _ = answer.send(RelayMsg::HubAnswer(id, settled.map_err(|_| ())));
                    }),
                );
            },
            move || {
                let _ = lost.send(RelayMsg::HubAnswer(id, Err(())));
            },
        );
    }

    fn cancel(&self, caller: &str, call_id: &str) {
        let (caller, call_id) = (caller.to_owned(), call_id.to_owned());
        post_core(move |provided, _| {
            if let Some(hub) = hub_of(provided) {
                hub.relay_cancel(&caller, &call_id);
            }
        });
    }
}

/// The agent window's card for a call of an `ask` tool
struct AskLink {
    asker: Arc<dyn RelayAsker>,
    tx: mpsc::Sender<RelayMsg>,
    /// the asker's token of each call waiting for the person
    tokens: RefCell<HashMap<u64, u64>>,
}

impl AgentRelayAsk for AskLink {
    fn ask(&self, call: RelayCallId, request: &AgentRelayAskRequest) {
        let tx = self.tx.clone();
        let token = self.asker.ask(
            RelayAskRequest {
                exposed_name: request.exposed_name.clone(),
                server: request.server.clone(),
                input: request.input.clone(),
            },
            Box::new(move |answer| {
                let answer = match answer {
                    RelayAnswer::Deny => AgentRelayAnswer::Deny,
                    RelayAnswer::Allow { scope: "session" } => {
                        AgentRelayAnswer::Allow(AgentRelayScope::Session)
                    }
                    RelayAnswer::Allow { .. } => AgentRelayAnswer::Allow(AgentRelayScope::Once),
                };
                let _ = tx.send(RelayMsg::AskAnswer(call, answer));
            }),
        );
        self.tokens.borrow_mut().insert(call.0, token);
    }

    fn abort(&self, call: RelayCallId) {
        if let Some(token) = self.tokens.borrow().get(&call.0) {
            self.asker.abort(*token);
        }
    }
}

struct RelayLoop {
    relays: AgentMcpRelays<'static>,
    transports: Vec<Transport>,
    tx: mpsc::Sender<RelayMsg>,
    ready: Option<mpsc::Sender<Result<(Vec<AgentMcpRelay>, Vec<String>), String>>>,
    done: bool,
}

impl RelayLoop {
    fn transport(&mut self, index: usize) -> &Transport {
        while self.transports.len() <= index {
            let at = self.transports.len();
            let sink = self.tx.clone();
            self.transports.push(Transport::new(Arc::new(move |event| {
                let _ = sink.send(RelayMsg::Event(at, event));
            })));
        }
        &self.transports[index]
    }

    /// Carries out a step and every step its commands lead to
    fn run(&mut self, step: RelayStep) {
        let mut queue: VecDeque<(usize, Command)> = VecDeque::new();
        self.absorb(step, &mut queue);
        while let Some((index, command)) = queue.pop_front() {
            let Err(command) = self.transport(index).execute(command) else {
                continue;
            };
            let next = match command {
                Command::StartSettled { outcome, .. } => self.relays.on_started(index, outcome),
                Command::StopSettled { stop } => self.relays.on_stopped(index, stop),
                Command::Aborted(aborted) => self.relays.on_aborted(index, &aborted),
                _ => continue,
            };
            self.absorb(next, &mut queue);
        }
    }

    fn absorb(&mut self, step: RelayStep, queue: &mut VecDeque<(usize, Command)>) {
        queue.extend(
            step.commands
                .into_iter()
                .map(|tagged| (tagged.service, tagged.command)),
        );
        match step.done {
            Some(RelayDone::Started(outcome)) => {
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(outcome.map(|()| {
                        let relays = self
                            .relays
                            .relays()
                            .iter()
                            .map(|relay| AgentMcpRelay {
                                name: relay.name.clone(),
                                url: relay.url.clone(),
                                token: relay.token.clone(),
                                tools: relay.tools.clone(),
                            })
                            .collect();
                        (relays, self.relays.keep_results_inside())
                    }));
                }
            }
            Some(RelayDone::Stopped) => self.done = true,
            None => {}
        }
    }

    fn message(&mut self, message: RelayMsg) {
        match message {
            RelayMsg::Event(index, event) => {
                if index >= self.transports.len() {
                    return;
                }
                let commands = route(
                    self.relays.service_mut(index),
                    &self.transports[index],
                    event,
                );
                let step = RelayStep {
                    commands: commands
                        .into_iter()
                        .map(
                            |command| main_core::shell::agent::agent_mcp_relay::ServiceCommand {
                                service: index,
                                command,
                            },
                        )
                        .collect(),
                    done: None,
                };
                self.run(step);
            }
            RelayMsg::HubAnswer(call_id, answer) => {
                let step = self.relays.on_hub_answer(&call_id, answer);
                self.run(step);
            }
            RelayMsg::AskAnswer(call, answer) => {
                let step = self.relays.on_ask_answer(call, Ok(answer));
                self.run(step);
            }
            RelayMsg::Stop => {
                let step = self.relays.stop();
                self.run(step);
            }
        }
    }
}

fn run_relays(
    version: String,
    caller: String,
    asker: Arc<dyn RelayAsker>,
    tx: mpsc::Sender<RelayMsg>,
    rx: &mpsc::Receiver<RelayMsg>,
    ready: mpsc::Sender<Result<(Vec<AgentMcpRelay>, Vec<String>), String>>,
) {
    let (relays, first) = AgentMcpRelays::start(AgentMcpRelaysDeps {
        hub: Rc::new(HubLink { tx: tx.clone() }),
        ask: Rc::new(AskLink {
            asker,
            tx: tx.clone(),
            tokens: RefCell::new(HashMap::new()),
        }),
        caller,
        version,
        host: Rc::new(AgentHost),
    });
    let mut running = RelayLoop {
        relays,
        transports: Vec::new(),
        tx,
        ready: Some(ready),
        done: false,
    };
    running.run(first);
    while !running.done {
        let Ok(message) = rx.recv() else {
            break;
        };
        running.message(message);
    }
    for transport in &running.transports {
        if let Err(error) = transport.shutdown_and_wait() {
            eprintln!("shell: agent MCP relay: stopping the transport: {error}");
        }
    }
}

/// `startAgentMcpRelays({ hub, ask, caller: 'agent:<sessionId>', version })`: a relay per server of
/// the hub that is on, started and waited for here (a worker)
pub(crate) fn start_agent_relays(
    version: String,
    session_id: &str,
    asker: Arc<dyn RelayAsker>,
) -> Result<RelaysStarted, String> {
    let (tx, rx) = mpsc::channel();
    let (ready, started) = mpsc::channel();
    let actor_tx = tx.clone();
    let failed = ready.clone();
    let caller = format!("agent:{session_id}");
    std::thread::Builder::new()
        .name("agent-mcp-relays".into())
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                run_relays(version, caller, asker, actor_tx, &rx, ready);
            }));
            if outcome.is_err() {
                let _ = failed.send(Err("agent MCP relays panicked".to_owned()));
            }
        })
        .map_err(|error| error.to_string())?;
    match started.recv() {
        Ok(Ok((relays, keep_results_inside))) => Ok(RelaysStarted {
            relays,
            keep_results_inside,
            stop: Box::new(move || {
                let _ = tx.send(RelayMsg::Stop);
            }),
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("agent MCP relays stopped before they started".to_owned()),
    }
}

/// Whether the hub of the person's servers is there to relay: the port is built on the core thread
pub(crate) fn has_hub() -> bool {
    on_core(|provided| hub_of(provided).is_some()).unwrap_or(false)
}
