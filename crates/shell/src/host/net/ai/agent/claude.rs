//! `packages/ai-provider/src/agent-session/claude.ts`: agent mode on Claude Code.
//! The CLI runs with its own file, command, skill and web tools in the project
//! folder and asks the app before it acts. One process serves the whole
//! conversation; each message the person sends is one turn over stdin.
//!
//! The runner is the TypeScript's, on the AI runtime the core thread drives
//! ([`Driver`]): the program is located, spawned and registered through the
//! same `Rt` calls the Codex runner uses, so the child's environment, process
//! group and shutdown are the ones every subscription program gets. What the
//! CLI writes reaches the session through `Child::on` listeners, a request's
//! promise is an `Rt::deferred`.
//!
//! The session lives on the core thread. [`run`] is called on a worker (the
//! start of an agent session) and waits for the core thread to start it; the
//! [`ClaudeSession`] it answers holds only the session's number, and each
//! method reaches the session there.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use jscompat::node_path::{host, posix, win32};
use jscompat::{JsonValue, json_parse, json_stringify, number_to_string, slice_utf16, trim};
use main_core::packages::ai_provider::agent_session_claude::{
    APP_MCP_SERVER_NAME, agent_usable_relays, is_agent_app_mcp_tool,
};
use main_core::packages::ai_provider::agent_session_codex_sandbox_probe::default_kill_tree;
use main_core::packages::ai_provider::agent_session_messages::{DECLINED_BY_PERSON, deny_message};
use main_core::packages::ai_provider::agent_session_policy::{
    AgentPolicyContext, agent_policy_verdict, web_host_of,
};
use main_core::packages::ai_provider::agent_session_types::{
    AgentAction, AgentActionKind, AgentAppMcp, AgentMcpRelay, AgentPermissionMode,
    AgentPermissionRequest, AgentPolicyVerdict, AgentSessionAllowances, AgentSessionEvent,
    AgentSessionOptions, DEFAULT_AGENT_PERMISSION_MODE, agent_permission_mode_of, new_agent_action,
};
use main_core::packages::ai_provider::claude_code_cli::{
    ClaudeCodeCliDeps, claude_code_effort, exit_failure, is_model_name, read_lines,
    reported_failure, start_failure,
};
use main_core::packages::ai_provider::jsval::{
    Obj, as_record, as_str, length16, object, prop, text,
};
use main_core::packages::ai_provider::run_error::ai_run_error;
use main_core::packages::ai_provider::subscription_locate::{
    SubscriptionLocateOptions, resolve_subscription_program,
};
use main_core::packages::ai_provider::subscription_process::{
    SubscriptionSpawnOptions, spawn_subscription_program,
};
use main_core::packages::ai_provider::task::{Child, ChildEvent, FsOp, Id, JsError, Rt, Settle};
use main_core::shell::agent::agent_sessions::AgentSessionOptions as ShellSessionOptions;
use shell_ipc_a::agent::{ClaudeSession, Fault, RunnerFault};
use shell_ipc_a::runtime::Driver;

use crate::host::w::{on_core_services_local, post_core, post_core_checked};

const PROVIDER: &str = "claude-code";

/// The CLI's own tools the session turns on; everything else stays off
pub const CLAUDE_AGENT_TOOLS: [&str; 10] = [
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "Bash",
    "PowerShell",
    "Skill",
    // the web, as in the program's own app: each asks the person first (see the policy)
    "WebSearch",
    "WebFetch",
];

const SETTINGS_FILE: &str = "claude-settings.json";
const MCP_CONFIG_FILE: &str = "claude-mcp.json";
const INITIALIZE_REQUEST_ID: &str = "initialize";
const DIAGNOSTIC_LIMIT: usize = 4_000;
/// a conversation left alone this long is ended
const DEFAULT_IDLE_MS: f64 = 30.0 * 60_000.0;
const INITIALIZE_TIMEOUT_MS: f64 = 30_000.0;

/// `claudeAgentPermissionMode(mode)`: the CLI's permission mode for the
/// person's choice. The first two run in the CLI's manual mode so that every
/// edit and command comes to the app, which applies its own policy. Trusting
/// the project runs in the CLI's `auto` mode.
pub fn permission_mode(mode: AgentPermissionMode) -> &'static str {
    if mode == AgentPermissionMode::TrustProject {
        "auto"
    } else {
        "default"
    }
}

/// `ClaudeAgentLaunch`
pub struct Launch<'a> {
    pub model: &'a str,
    /// a level of `CLAUDE_EFFORTS`; anything else is left out
    pub effort: Option<&'a str>,
    pub mode: AgentPermissionMode,
    pub settings_file: &'a str,
    pub mcp_config_file: Option<&'a str>,
    pub skill_bundle_dir: Option<&'a str>,
}

/// `claudeAgentLaunchArgs(launch)`
pub fn launch_args(launch: &Launch<'_>) -> Result<Vec<String>, JsError> {
    let model = trim(launch.model);
    if !model.is_empty() && !is_model_name(model) {
        return Err(JsError::error(format!(
            "Invalid Claude Code model name: {}",
            slice_utf16(model, 0.0, Some(80.0))
        )));
    }
    let effort_value = launch.effort.map(text);
    let effort = claude_code_effort(effort_value.as_ref());
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--restricted",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--tools",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    args.push(CLAUDE_AGENT_TOOLS.join(","));
    // both are needed for the CLI to send its permission prompts over stdout
    for arg in [
        "--permission-prompts",
        "host",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
    ] {
        args.push(arg.to_owned());
    }
    args.push(permission_mode(launch.mode).to_owned());
    args.push("--settings".to_owned());
    args.push(launch.settings_file.to_owned());
    if let Some(file) = launch.mcp_config_file {
        args.push("--mcp-config".to_owned());
        args.push(file.to_owned());
    }
    // a skill's scripts live in the copy, which must count as a working folder to run
    if let Some(dir) = launch.skill_bundle_dir {
        for arg in ["--plugin-dir", dir, "--add-dir", dir] {
            args.push(arg.to_owned());
        }
    }
    args.push("--no-session-persistence".to_owned());
    if !model.is_empty() {
        args.push("--model".to_owned());
        args.push(model.to_owned());
    }
    if let Some(effort) = effort {
        args.push("--effort".to_owned());
        args.push(effort.to_owned());
    }
    Ok(args)
}

/// `claudeRulePath(path, platform)`: an absolute path in the CLI's
/// permission-rule form: `//` and the POSIX path, with a Windows drive written
/// as its lowercase letter (`C:\a` becomes `//c/a`). `None` for a path a rule
/// cannot carry (parentheses end the rule, glob characters would widen it);
/// such a path is still judged by the app's policy whenever the CLI asks.
pub fn rule_path(path: &str, platform: &str) -> Option<String> {
    if path.chars().any(|c| "()*?[]{}".contains(c)) {
        return None;
    }
    if platform == "win32" {
        let normal = win32::normalize(path);
        let mut chars = normal.chars();
        let drive = chars.next().filter(char::is_ascii_alphabetic)?;
        let rest = normal.get(1..)?.strip_prefix(":\\")?;
        let rest: Vec<&str> = rest.split('\\').filter(|part| !part.is_empty()).collect();
        return Some(format!(
            "//{}/{}",
            drive.to_ascii_lowercase(),
            rest.join("/")
        ));
    }
    let normal = posix::normalize(path);
    normal.starts_with('/').then(|| format!("/{normal}"))
}

/// `claudeAgentSettings(options, platform)`: the settings the session adds on
/// top of no settings at all. Reads outside the working folders ask even in
/// `auto` mode; protected folders cannot be read or written; documents open
/// in tabs cannot be written by the file tools.
pub fn settings(options: &AgentSessionOptions, platform: &str) -> JsonValue {
    let mut deny: Vec<JsonValue> = Vec::new();
    for path in &options.protected_paths {
        let Some(rule) = rule_path(path, platform) else {
            continue;
        };
        for tool in ["Read", "Edit", "Write"] {
            deny.push(text(format!("{tool}({rule})")));
            deny.push(text(format!("{tool}({rule}/**)")));
        }
    }
    for path in &options.open_documents {
        let Some(rule) = rule_path(path, platform) else {
            continue;
        };
        deny.push(text(format!("Edit({rule})")));
        deny.push(text(format!("Write({rule})")));
    }
    // In `auto` mode the CLI's own reviewer judges a call before it reaches the app; a relay asks
    // the person itself (user MCP spec 9), so its calls are let through to it rather than asked twice
    let relays = if options.mode == AgentPermissionMode::TrustProject {
        agent_usable_relays(options.user_mcp_relays.as_deref())
    } else {
        Vec::new()
    };
    let mut permissions = vec![("blockReadsOutsideWorkingDirectories", JsonValue::Bool(true))];
    if !relays.is_empty() {
        permissions.push((
            "allow",
            JsonValue::Array(
                relays
                    .iter()
                    .map(|relay| text(format!("mcp__{}", relay.name)))
                    .collect(),
            ),
        ));
    }
    permissions.push(("deny", JsonValue::Array(deny)));
    object(vec![("permissions", object(permissions))])
}

/// `claudeAgentMcpConfig(appMcp, relays)`: the session's MCP servers, the
/// app's own and a relay per person's server. Each entry holds a loopback
/// address and a token of this session only.
pub fn mcp_config(app_mcp: Option<&AgentAppMcp>, relays: &[AgentMcpRelay]) -> JsonValue {
    let entry = |url: &str, token: &str| {
        object(vec![
            ("type", text("http")),
            ("url", text(url)),
            (
                "headers",
                object(vec![("Authorization", text(format!("Bearer {token}")))]),
            ),
        ])
    };
    let mut servers: Vec<(&str, JsonValue)> = Vec::new();
    for relay in relays {
        servers.push((relay.name.as_str(), entry(&relay.url, &relay.token)));
    }
    if let Some(app) = app_mcp {
        servers.push((APP_MCP_SERVER_NAME, entry(&app.url, &app.token)));
    }
    object(vec![("mcpServers", object(servers))])
}

/// `stringField(input, ...names)`: the first of the names that holds a string that is not empty
fn string_field(input: Option<&JsonValue>, names: &[&str]) -> Option<String> {
    let input = input?;
    names.iter().find_map(|name| match prop(input, name) {
        Some(JsonValue::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    })
}

/// `agentRelayToolOf(name, relays)`: the relay a program-side tool name
/// `mcp__<relay>__<tool>` belongs to, with the bare tool
fn relay_tool_of(name: &str, relays: &[AgentMcpRelay]) -> Option<(AgentMcpRelay, String)> {
    agent_usable_relays(Some(relays))
        .into_iter()
        .find_map(|relay| {
            let tool = name
                .strip_prefix(format!("mcp__{}__", relay.name).as_str())?
                .to_owned();
            Some((relay, tool))
        })
}

/// `claudeAgentAction(tool, input, relays)`: what a Claude Code tool call does,
/// in the policy's terms. A tool of this session's relays is an app tool: the
/// relay asks the person when its permission says so, and the program-level
/// card would ask a second time.
pub fn action(tool: &str, input: Option<&JsonValue>, relays: &[AgentMcpRelay]) -> AgentAction {
    let reason = string_field(input, &["description"]);
    if tool.starts_with(format!("mcp__{APP_MCP_SERVER_NAME}__").as_str())
        || relay_tool_of(tool, relays).is_some()
    {
        return new_agent_action(AgentActionKind::AppTool, tool, Vec::new());
    }
    let with_paths = |kind: AgentActionKind, path: Option<String>| {
        let mut action = new_agent_action(kind, tool, path.into_iter().collect());
        action.reason = reason.clone();
        action
    };
    match tool {
        "Read" | "Glob" | "Grep" => with_paths(
            AgentActionKind::Read,
            string_field(input, &["file_path", "path"]),
        ),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => with_paths(
            AgentActionKind::Write,
            string_field(input, &["file_path", "notebook_path"]),
        ),
        "Bash" | "PowerShell" => {
            let mut action = with_paths(AgentActionKind::Command, None);
            action.command = Some(string_field(input, &["command"]).unwrap_or_default());
            action
        }
        "Skill" => new_agent_action(AgentActionKind::Skill, tool, Vec::new()),
        "WebSearch" => {
            let mut action = new_agent_action(AgentActionKind::WebSearch, tool, Vec::new());
            action.target = string_field(input, &["query"]);
            action
        }
        "WebFetch" => {
            let url = string_field(input, &["url"]);
            let mut action = new_agent_action(AgentActionKind::WebFetch, tool, Vec::new());
            action.host = web_host_of(url.as_deref());
            action.target = url;
            action
        }
        _ => with_paths(AgentActionKind::Other, None),
    }
}

/// `claudeAgentInitProblem(init, relays)`: the tool list and MCP servers the
/// CLI reported are the ones the session asked for: its fixed tools, the app
/// server's tools, and this session's relays with the tools each listed at the
/// start. Anything else is a problem.
pub fn init_problem(init: &JsonValue, relays: &[AgentMcpRelay]) -> Option<String> {
    let Some(JsonValue::Array(tools)) = prop(init, "tools") else {
        return Some("Claude Code did not report its tool list".to_owned());
    };
    let app_prefix = format!("mcp__{APP_MCP_SERVER_NAME}__");
    let relay_tool = |tool: &str| {
        relay_tool_of(tool, relays).is_some_and(|(relay, bare)| relay.tools.contains(&bare))
    };
    // the app's own tools by name: a tool of its server the session was not meant to get is extra
    let extra = tools
        .iter()
        .filter(|tool| match tool {
            JsonValue::String(tool) => {
                !(CLAUDE_AGENT_TOOLS.contains(&tool.as_str())
                    || (tool.starts_with(&app_prefix) && is_agent_app_mcp_tool(tool))
                    || relay_tool(tool))
            }
            _ => true,
        })
        .count();
    if extra > 0 {
        return Some(format!(
            "Claude Code started with tools the app did not turn on: {extra}"
        ));
    }
    let mut given = vec![APP_MCP_SERVER_NAME.to_owned()];
    given.extend(
        agent_usable_relays(Some(relays))
            .into_iter()
            .map(|relay| relay.name),
    );
    let foreign = match prop(init, "mcp_servers") {
        Some(JsonValue::Array(servers)) => servers
            .iter()
            .filter(|server| match prop(server, "name") {
                Some(JsonValue::String(name)) if as_record(server).is_some() => {
                    !given.contains(name)
                }
                _ => true,
            })
            .count(),
        _ => 0,
    };
    (foreign > 0).then(|| "Claude Code started with an MCP server the app did not give it".into())
}

/// `String(value)` of a value a CLI line carries
fn js_string(value: Option<&JsonValue>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(JsonValue::String(value)) => value.clone(),
        Some(JsonValue::Null) => "null".to_owned(),
        Some(JsonValue::Bool(flag)) => flag.to_string(),
        Some(JsonValue::Number(number)) => number_to_string(*number),
        Some(JsonValue::Array(items)) => items
            .iter()
            .map(|item| match item {
                JsonValue::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(JsonValue::Object(_)) => "[object Object]".to_owned(),
    }
}

/// the kind an `AiRunError` carries
fn run_error_kind(error: &JsError) -> Option<String> {
    if error.is("AiRunError") {
        as_str(error.prop("kind")).map(str::to_owned)
    } else {
        None
    }
}

/// a permission prompt waiting for the person
struct Pending {
    input: Option<JsonValue>,
    tool_use_id: Option<String>,
    action: AgentAction,
}

struct St {
    open_documents: Vec<String>,
    ended: bool,
    stopping: bool,
    verified: bool,
    busy: bool,
    stderr: String,
    failure_code: Option<String>,
    failure_text: String,
    rate_limit_info: Option<Obj>,
    idle_timer: Option<Id>,
    /// permission prompts waiting for the person, by request id
    pending: HashMap<String, Pending>,
    /// what the person allowed for the rest of this session; memory only, never written
    allowances: AgentSessionAllowances,
    /// tool uses by id, so their results can be shown against them
    tool_uses: HashMap<String, AgentAction>,
    denied: HashSet<String>,
}

/// What the closures of `startClaudeAgentSession` share
struct Core {
    rt: Rt,
    platform: String,
    options: AgentSessionOptions,
    relays: Vec<AgentMcpRelay>,
    child: Child,
    on_event: Rc<dyn Fn(&AgentSessionEvent)>,
    kill_tree: Rc<dyn Fn(Option<f64>)>,
    idle_ms: f64,
    setup: Settle<Result<(), JsError>>,
    setup_timer: Id,
    st: RefCell<St>,
}

impl Core {
    /// `emit(event)`
    fn emit(&self, event: AgentSessionEvent) {
        if !self.st.borrow().ended {
            (self.on_event)(&event);
        }
    }

    /// `send(message)` of the TypeScript's closure: one JSON line to the CLI
    fn write_line(&self, message: &JsonValue) {
        if self.child.stdin_writable() {
            self.child
                .write(format!("{}\n", json_stringify(message)).as_bytes());
        }
    }

    /// `end(error)`
    fn end(&self, error: Option<JsError>) {
        if self.st.borrow().ended {
            return;
        }
        if let Some(error) = &error {
            self.emit(AgentSessionEvent::Error {
                kind: run_error_kind(error),
                message: error.message.clone(),
            });
        }
        let idle_timer = {
            let mut st = self.st.borrow_mut();
            st.ended = true;
            st.pending.clear();
            st.idle_timer
        };
        self.rt.clear_timeout(idle_timer);
        (self.on_event)(&AgentSessionEvent::Ended);
    }

    /// `stopNow(error)`
    fn stop_now(&self, error: Option<JsError>) {
        self.st.borrow_mut().stopping = true;
        (self.kill_tree)(self.child.pid());
        self.end(error);
    }

    /// `armIdle()`
    fn arm_idle(self: &Rc<Self>) {
        let previous = self.st.borrow().idle_timer;
        self.rt.clear_timeout(previous);
        let core = self.clone();
        let timer = self
            .rt
            .set_timeout(self.idle_ms, move || core.stop_now(None));
        self.st.borrow_mut().idle_timer = Some(timer);
    }

    /// `initialized(ok, error)`
    fn initialized(&self, result: Result<(), JsError>) {
        self.rt.clear_timeout(Some(self.setup_timer));
        self.setup.settle(result);
    }

    /// `respond(requestId, response)`
    fn respond(&self, request_id: &str, response: JsonValue) {
        self.write_line(&object(vec![
            ("type", text("control_response")),
            (
                "response",
                object(vec![
                    ("subtype", text("success")),
                    ("request_id", text(request_id)),
                    ("response", response),
                ]),
            ),
        ]));
    }

    /// `allow(requestId, input)`
    fn allow(&self, request_id: &str, input: Option<&JsonValue>) {
        let updated = match input {
            Some(input @ JsonValue::Object(_)) => input.clone(),
            _ => object(Vec::new()),
        };
        self.respond(
            request_id,
            object(vec![("behavior", text("allow")), ("updatedInput", updated)]),
        );
    }

    /// `deny(requestId, message)`
    fn deny(&self, request_id: &str, message: &str) {
        self.respond(
            request_id,
            object(vec![("behavior", text("deny")), ("message", text(message))]),
        );
    }

    /// `onPermissionRequest(requestId, request)`
    fn on_permission_request(&self, request_id: &str, request: &JsonValue) {
        if !self.st.borrow().verified {
            self.stop_now(Some(ai_run_error(
                "unsafe-tools",
                "Claude Code asked for a tool before it reported its tool list",
                None,
                None,
            )));
            return;
        }
        let tool = as_str(prop(request, "tool_name")).unwrap_or_default();
        let input = prop(request, "input");
        let mut action = action(tool, input, &self.relays);
        if let Some(reason) = as_str(prop(request, "decision_reason"))
            && action.reason.as_deref().is_none_or(str::is_empty)
        {
            action.reason = Some(reason.to_owned());
        }
        let tool_use_id = as_str(prop(request, "tool_use_id"))
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let verdict = {
            let st = self.st.borrow();
            agent_policy_verdict(
                &action,
                &AgentPolicyContext {
                    mode: self.options.mode,
                    project_root: &self.options.project_root,
                    open_documents: &st.open_documents,
                    protected_paths: &self.options.protected_paths,
                    skill_bundle_dir: self.options.skill_bundle_dir.as_deref(),
                    allowances: Some(&st.allowances),
                    platform: Some(&self.platform),
                },
            )
        };
        match verdict {
            AgentPolicyVerdict::Allow => self.allow(request_id, input),
            AgentPolicyVerdict::Deny(reason) => {
                if let Some(id) = tool_use_id {
                    self.st.borrow_mut().denied.insert(id);
                }
                self.deny(request_id, deny_message(reason));
            }
            AgentPolicyVerdict::Ask => {
                {
                    let mut st = self.st.borrow_mut();
                    st.pending.insert(
                        request_id.to_owned(),
                        Pending {
                            input: input.cloned(),
                            tool_use_id: tool_use_id.clone(),
                            action: action.clone(),
                        },
                    );
                    if let Some(id) = tool_use_id {
                        st.tool_uses.insert(id, action.clone());
                    }
                }
                self.emit(AgentSessionEvent::Permission {
                    request: AgentPermissionRequest {
                        action,
                        id: request_id.to_owned(),
                        provider: PROVIDER.to_owned(),
                    },
                });
            }
        }
    }

    /// `onAssistant(event)`
    fn on_assistant(&self, event: &JsonValue) {
        let content = prop(event, "message").and_then(|message| prop(message, "content"));
        if let Some(JsonValue::String(code)) = prop(event, "error") {
            let joined = match content {
                Some(JsonValue::Array(blocks)) => blocks
                    .iter()
                    .map(|block| as_str(prop(block, "text")).unwrap_or_default())
                    .collect::<String>(),
                _ => String::new(),
            };
            let mut st = self.st.borrow_mut();
            st.failure_code = Some(code.clone());
            st.failure_text = joined;
            return;
        }
        if !self.st.borrow().verified {
            self.stop_now(Some(ai_run_error(
                "unsafe-tools",
                "Claude Code answered before it reported its tool list",
                None,
                None,
            )));
            return;
        }
        let Some(JsonValue::Array(blocks)) = content else {
            return;
        };
        for block in blocks {
            if as_record(block).is_none() {
                continue;
            }
            let kind = as_str(prop(block, "type"));
            if kind == Some("text")
                && let Some(value) = as_str(prop(block, "text"))
                && !trim(value).is_empty()
            {
                self.emit(AgentSessionEvent::Text {
                    text: value.to_owned(),
                });
            } else if kind == Some("tool_use")
                && let Some(name) = as_str(prop(block, "name"))
            {
                let action = action(name, prop(block, "input"), &self.relays);
                if let Some(id) = as_str(prop(block, "id")) {
                    self.st
                        .borrow_mut()
                        .tool_uses
                        .insert(id.to_owned(), action.clone());
                }
                self.emit(AgentSessionEvent::Tool {
                    action,
                    status: "running",
                });
            }
        }
    }

    /// `onUser(event)`
    fn on_user(&self, event: &JsonValue) {
        let Some(JsonValue::Array(blocks)) =
            prop(event, "message").and_then(|message| prop(message, "content"))
        else {
            return;
        };
        for block in blocks {
            if as_record(block).is_none() || as_str(prop(block, "type")) != Some("tool_result") {
                continue;
            }
            let id = as_str(prop(block, "tool_use_id")).unwrap_or_default();
            let (action, was_denied) = {
                let mut st = self.st.borrow_mut();
                let Some(action) = st.tool_uses.remove(id) else {
                    continue;
                };
                (action, st.denied.remove(id))
            };
            let status = if was_denied {
                "denied"
            } else if matches!(prop(block, "is_error"), Some(JsonValue::Bool(true))) {
                "failed"
            } else {
                "done"
            };
            self.emit(AgentSessionEvent::Tool { action, status });
        }
    }

    /// `onResult(event)`
    fn on_result(self: &Rc<Self>, event: &JsonValue) {
        self.st.borrow_mut().busy = false;
        self.arm_idle();
        let failed = matches!(prop(event, "is_error"), Some(JsonValue::Bool(true)))
            || as_str(prop(event, "subtype")) != Some("success");
        if !failed {
            self.emit(AgentSessionEvent::TurnDone { ok: true });
            return;
        }
        let errors = match prop(event, "errors") {
            Some(JsonValue::Array(items)) => items
                .iter()
                .map(|item| match item {
                    JsonValue::Null => String::new(),
                    other => js_string(Some(other)),
                })
                .collect::<Vec<_>>()
                .join("; "),
            _ => String::new(),
        };
        let result = as_str(prop(event, "result")).unwrap_or_default().to_owned();
        let (code, failure_text, rate_limit_info) = {
            let mut st = self.st.borrow_mut();
            (
                st.failure_code.take(),
                std::mem::take(&mut st.failure_text),
                st.rate_limit_info.clone(),
            )
        };
        let message = [result, failure_text, errors]
            .into_iter()
            .find(|candidate| !candidate.is_empty())
            .unwrap_or_default();
        let error = reported_failure(
            code.as_deref(),
            prop(event, "api_error_status"),
            &message,
            rate_limit_info.as_ref(),
        );
        self.emit(AgentSessionEvent::TurnDone { ok: false });
        self.emit(AgentSessionEvent::Error {
            kind: run_error_kind(&error),
            message: error.message,
        });
    }

    /// `onLine(line)`
    fn on_line(self: &Rc<Self>, line: &str) {
        if self.st.borrow().ended {
            return;
        }
        let Ok(event @ JsonValue::Object(_)) = json_parse(line) else {
            return;
        };
        match as_str(prop(&event, "type")) {
            Some("control_response") => {
                let Some(response) = prop(&event, "response").filter(|r| as_record(r).is_some())
                else {
                    return;
                };
                if as_str(prop(response, "request_id")) != Some(INITIALIZE_REQUEST_ID) {
                    return;
                }
                if as_str(prop(response, "subtype")) == Some("success") {
                    self.initialized(Ok(()));
                } else {
                    self.initialized(Err(ai_run_error(
                        "needs-update",
                        "Claude Code did not accept the session setup of this app; it may be too old",
                        Some(PROVIDER),
                        None,
                    )));
                }
            }
            Some("control_request") => {
                let request_id = as_str(prop(&event, "request_id")).unwrap_or_default();
                let request = prop(&event, "request").filter(|r| as_record(r).is_some());
                let subtype = request.and_then(|request| prop(request, "subtype"));
                if let Some(request) = request
                    && as_str(subtype) == Some("can_use_tool")
                    && !request_id.is_empty()
                {
                    self.on_permission_request(request_id, request);
                    return;
                }
                // no hook is registered and the app's MCP server is reached over HTTP, so nothing else
                // is expected; answer instead of leaving the CLI waiting
                self.write_line(&object(vec![
                    ("type", text("control_response")),
                    (
                        "response",
                        object(vec![
                            ("subtype", text("error")),
                            ("request_id", text(request_id)),
                            (
                                "error",
                                text(format!("Unsupported request: {}", js_string(subtype))),
                            ),
                        ]),
                    ),
                ]));
            }
            Some("system") => {
                if as_str(prop(&event, "subtype")) != Some("init") {
                    return;
                }
                if let Some(problem) = init_problem(&event, &self.relays) {
                    self.stop_now(Some(ai_run_error(
                        "unsafe-tools",
                        format!("{problem}; the session was stopped"),
                        None,
                        None,
                    )));
                    return;
                }
                self.st.borrow_mut().verified = true;
                let skills = match prop(&event, "skills") {
                    Some(JsonValue::Array(skills)) => skills
                        .iter()
                        .filter_map(|skill| match skill {
                            JsonValue::String(skill) => Some(skill.clone()),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                self.emit(AgentSessionEvent::Started {
                    provider: PROVIDER.to_owned(),
                    model: as_str(prop(&event, "model")).map(str::to_owned),
                    skills,
                });
            }
            Some("rate_limit_event") => {
                if let Some(JsonValue::Object(info)) = prop(&event, "rate_limit_info") {
                    self.st.borrow_mut().rate_limit_info = Some(info.clone());
                }
            }
            Some("assistant") => self.on_assistant(&event),
            Some("user") => self.on_user(&event),
            Some("result") => self.on_result(&event),
            _ => {}
        }
    }
}

/// The running session `startClaudeAgentSession` answers
pub struct ClaudeAgentSession {
    core: Rc<Core>,
}

impl ClaudeAgentSession {
    /// `send(text)`
    pub fn send(&self, message: &str) -> Result<(), JsError> {
        {
            let mut st = self.core.st.borrow_mut();
            if st.ended {
                return Err(JsError::error("The agent session has ended"));
            }
            if st.busy {
                return Err(JsError::error(
                    "The agent is still answering the previous message",
                ));
            }
            st.busy = true;
        }
        let idle_timer = self.core.st.borrow().idle_timer;
        self.core.rt.clear_timeout(idle_timer);
        self.core.write_line(&object(vec![
            ("type", text("user")),
            (
                "message",
                object(vec![
                    ("role", text("user")),
                    (
                        "content",
                        JsonValue::Array(vec![object(vec![
                            ("type", text("text")),
                            ("text", text(message)),
                        ])]),
                    ),
                ]),
            ),
        ]));
        Ok(())
    }

    /// `answer(id, answer)`
    pub fn answer(&self, id: &str, allow: bool, scope: Option<&str>, message: Option<&str>) {
        let request = self.core.st.borrow_mut().pending.remove(id);
        let Some(request) = request else {
            return;
        };
        if allow {
            if scope == Some("session") {
                let mut st = self.core.st.borrow_mut();
                match request.action.kind {
                    AgentActionKind::WebSearch => st.allowances.web_search = true,
                    AgentActionKind::WebFetch => {
                        if let Some(host) = request.action.host.as_ref().filter(|h| !h.is_empty()) {
                            st.allowances.web_hosts.insert(host.clone());
                        }
                    }
                    _ => {}
                }
            }
            self.core.allow(id, request.input.as_ref());
            return;
        }
        if let Some(tool_use_id) = request.tool_use_id {
            self.core.st.borrow_mut().denied.insert(tool_use_id);
        }
        self.core.deny(id, message.unwrap_or(DECLINED_BY_PERSON));
    }

    /// `setOpenDocuments(paths)`
    pub fn set_open_documents(&self, paths: &[String]) {
        self.core.st.borrow_mut().open_documents = paths.to_vec();
    }

    /// `stop()`
    pub fn stop(&self) {
        if !self.core.st.borrow().ended {
            self.core.stop_now(None);
        }
    }
}

/// `startClaudeAgentSession(options, onEvent)`
pub async fn start_claude_agent_session(
    rt: &Rt,
    deps: &ClaudeCodeCliDeps,
    options: &AgentSessionOptions,
    on_event: Rc<dyn Fn(&AgentSessionEvent)>,
) -> Result<ClaudeAgentSession, JsError> {
    let platform = rt.platform();
    let relays = agent_usable_relays(options.user_mcp_relays.as_deref());
    let settings_file = host::join(&[&options.session_dir, SETTINGS_FILE]);
    write_json(rt, &settings_file, &settings(options, &platform)).await?;
    let mcp_config_file = if options.app_mcp.is_some() || !relays.is_empty() {
        let file = host::join(&[&options.session_dir, MCP_CONFIG_FILE]);
        write_json(rt, &file, &mcp_config(options.app_mcp.as_ref(), &relays)).await?;
        Some(file)
    } else {
        None
    };
    let args = launch_args(&Launch {
        model: &options.model,
        effort: options.effort.as_deref(),
        mode: options.mode,
        settings_file: &settings_file,
        mcp_config_file: mcp_config_file.as_deref(),
        skill_bundle_dir: options.skill_bundle_dir.as_deref(),
    })?;
    let program = match &deps.resolve_program {
        Some(resolve) => resolve(options.cli_path.clone()).await?,
        None => {
            resolve_subscription_program(
                rt,
                &deps.locate,
                PROVIDER,
                &SubscriptionLocateOptions {
                    configured_path: options.cli_path.clone(),
                    ..SubscriptionLocateOptions::default()
                },
            )
            .await?
        }
    };
    let child = spawn_subscription_program(
        rt,
        &deps.shared,
        &program,
        &args,
        &SubscriptionSpawnOptions {
            cwd: Some(options.project_root.clone()),
            base_env: deps.base_env.clone(),
            registry: deps.registry.clone(),
            platform: Some(platform.clone()),
            ..SubscriptionSpawnOptions::default()
        },
    )
    .await
    .map_err(|error| start_failure(&error))?;
    let kill_tree = match &deps.kill_tree {
        Some(kill) => kill.clone(),
        None => default_kill_tree(rt),
    };

    let (setup, ready) = rt.deferred::<Result<(), JsError>>();
    let timeout = setup.clone();
    let setup_timer = rt.set_timeout(INITIALIZE_TIMEOUT_MS, move || {
        timeout.settle(Err(JsError::error(
            "Claude Code did not accept the session setup in time",
        )));
    });
    let core = Rc::new(Core {
        rt: rt.clone(),
        platform,
        options: options.clone(),
        relays,
        child: child.clone(),
        on_event,
        kill_tree,
        idle_ms: DEFAULT_IDLE_MS,
        setup,
        setup_timer,
        st: RefCell::new(St {
            open_documents: options.open_documents.clone(),
            ended: false,
            stopping: false,
            verified: false,
            busy: false,
            stderr: String::new(),
            failure_code: None,
            failure_text: String::new(),
            rate_limit_info: None,
            idle_timer: None,
            pending: HashMap::new(),
            allowances: AgentSessionAllowances::default(),
            tool_uses: HashMap::new(),
            denied: HashSet::new(),
        }),
    });

    {
        let on_line = core.clone();
        let overflow = core.clone();
        read_lines(
            &child,
            move |line| on_line.on_line(line),
            move || {
                overflow.stop_now(Some(JsError::error(
                    "Claude Code wrote a line too long to read; the session was stopped",
                )));
            },
        );
    }
    {
        let core = core.clone();
        let errored = Cell::new(false);
        let closed = Cell::new(false);
        child.on(move |event| match event {
            ChildEvent::Stderr(chunk) => {
                let mut st = core.st.borrow_mut();
                let next = format!("{}{}", st.stderr, String::from_utf8_lossy(chunk));
                st.stderr = if length16(&next) <= DIAGNOSTIC_LIMIT {
                    next
                } else {
                    slice_utf16(&next, -(DIAGNOSTIC_LIMIT as f64), None)
                };
            }
            ChildEvent::Error(error) => {
                if errored.replace(true) {
                    return;
                }
                let failure = start_failure(error);
                core.initialized(Err(failure.clone()));
                core.end(Some(failure));
            }
            ChildEvent::Close { code, .. } => {
                if closed.replace(true) {
                    return;
                }
                let (stderr, stopping) = {
                    let st = core.st.borrow();
                    (st.stderr.clone(), st.stopping)
                };
                let error = (!stopping).then(|| exit_failure(*code, &stderr));
                core.initialized(Err(error
                    .clone()
                    .unwrap_or_else(|| JsError::error("Claude Code ended"))));
                core.end(error);
            }
            ChildEvent::Stdout(_) | ChildEvent::Exit { .. } => {}
        });
    }

    core.write_line(&object(vec![
        ("type", text("control_request")),
        ("request_id", text(INITIALIZE_REQUEST_ID)),
        (
            "request",
            object(vec![
                ("subtype", text("initialize")),
                ("appendSystemPrompt", text(&options.instructions)),
            ]),
        ),
    ]));
    if let Err(error) = ready.await {
        core.stop_now(None);
        return Err(error);
    }
    core.arm_idle();
    Ok(ClaudeAgentSession { core })
}

/// `writeFile(path, JSON.stringify(value), 'utf8')`
async fn write_json(rt: &Rt, path: &str, value: &JsonValue) -> Result<(), JsError> {
    rt.fs(FsOp::WriteFile {
        path: path.to_owned(),
        data: json_stringify(value).into_bytes(),
        mode: None,
        flag: None,
    })
    .await
    .map(|_| ())
}

// ---- the worker's side: a session is reached by its number ----

thread_local! {
    /// The running sessions, on the core thread
    static SESSIONS: RefCell<HashMap<u64, ClaudeAgentSession>> = RefCell::new(HashMap::new());
    static NEXT_SESSION: Cell<u64> = const { Cell::new(1) };
}

/// Takes what a slot of a once-only job holds
fn take_slot<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
}

/// Runs `job` with session `id` on the core thread: at once there, else posted
fn with_session(id: u64, job: impl FnOnce(&ClaudeAgentSession) + Send + 'static) {
    let run = move || {
        let session = SESSIONS.with(|sessions| {
            sessions
                .borrow()
                .get(&id)
                .map(|session| ClaudeAgentSession {
                    core: session.core.clone(),
                })
        });
        if let Some(session) = session {
            job(&session);
        }
    };
    let slot = Arc::new(Mutex::new(Some(run)));
    let local = slot.clone();
    let ran = on_core_services_local(move |_, _| {
        if let Some(run) = take_slot(&local) {
            run();
        }
    });
    if !ran {
        post_core(move |_, _| {
            if let Some(run) = take_slot(&slot) {
                run();
            }
        });
    }
}

/// What a thrown value is to the manager: the `kind` of a run error, or just an `Error`
fn fault_of(error: &JsError) -> RunnerFault {
    let word = |key: &str| as_str(error.prop(key)).map(str::to_owned);
    RunnerFault {
        problem: None,
        kind: word("kind"),
        fault: Fault {
            name: error.name.clone(),
            code: word("code"),
            message: error.message.clone(),
        },
    }
}

fn unavailable() -> RunnerFault {
    RunnerFault {
        problem: None,
        kind: None,
        fault: Fault::new("Core services are not available"),
    }
}

/// The session options as the runner takes them
fn session_options(options: &ShellSessionOptions) -> Result<AgentSessionOptions, RunnerFault> {
    let mode = agent_permission_mode_of(&options.mode)
        .or_else(|| agent_permission_mode_of(DEFAULT_AGENT_PERMISSION_MODE))
        .ok_or_else(|| RunnerFault {
            problem: None,
            kind: None,
            fault: Fault::new("the permission mode is not known"),
        })?;
    Ok(AgentSessionOptions {
        project_root: options.project_root.clone(),
        mode,
        model: options.model.clone(),
        effort: options.effort.clone(),
        cli_path: options.cli_path.clone(),
        instructions: options.instructions.clone(),
        open_documents: options.open_documents.clone(),
        protected_paths: options.protected_paths.clone(),
        skill_bundle_dir: options.skill_bundle_dir.clone(),
        app_mcp: options.app_mcp.as_ref().map(|(url, token)| AgentAppMcp {
            url: url.clone(),
            token: token.clone(),
        }),
        user_mcp_relays: options.user_mcp_relays.as_ref().map(|relays| {
            relays
                .iter()
                .map(|relay| AgentMcpRelay {
                    name: relay.name.clone(),
                    url: relay.url.clone(),
                    token: relay.token.clone(),
                    tools: relay.tools.clone(),
                })
                .collect()
        }),
        session_dir: options.session_dir.clone(),
    })
}

/// A started session as the manager holds it
struct Handle {
    id: u64,
}

impl ClaudeSession for Handle {
    fn send(&self, message: &str) -> Result<(), String> {
        let (reply, answer) = mpsc::channel();
        let message = message.to_owned();
        let id = self.id;
        let slot = Arc::new(Mutex::new(Some(reply)));
        let lost = slot.clone();
        post_core_checked(
            move |_, _| {
                let Some(reply) = slot.lock().ok().and_then(|mut slot| slot.take()) else {
                    return;
                };
                let known = SESSIONS.with(|sessions| {
                    sessions
                        .borrow()
                        .get(&id)
                        .map(|session| session.send(&message))
                });
                let _ = reply.send(match known {
                    Some(sent) => sent.map_err(|error| error.message),
                    None => Err("The agent session has ended".to_owned()),
                });
            },
            move || {
                if let Some(reply) = lost.lock().ok().and_then(|mut slot| slot.take()) {
                    let _ = reply.send(Err("Core services are not available".to_owned()));
                }
            },
        );
        answer
            .recv()
            .unwrap_or_else(|_| Err("Core services are not available".to_owned()))
    }

    fn answer(&self, id: &str, allow: bool, scope: Option<&str>, message: Option<&str>) {
        let (id, scope, message) = (
            id.to_owned(),
            scope.map(str::to_owned),
            message.map(str::to_owned),
        );
        with_session(self.id, move |session| {
            session.answer(&id, allow, scope.as_deref(), message.as_deref());
        });
    }

    fn set_open_documents(&self, paths: &[String]) {
        let paths = paths.to_vec();
        with_session(self.id, move |session| session.set_open_documents(&paths));
    }

    fn stop(&self) {
        with_session(self.id, ClaudeAgentSession::stop);
    }
}

/// `startClaudeAgentSession(options, onEvent)` for a worker: the session is
/// started on the core thread's driver and waited for here. `on_event` takes
/// each event as its JSON, from the core thread.
pub fn run(
    options: &ShellSessionOptions,
    on_event: Arc<dyn Fn(JsonValue) + Send + Sync>,
) -> Result<Arc<dyn ClaudeSession>, RunnerFault> {
    let options = session_options(options)?;
    let (reply, answer) = mpsc::channel();
    let slot = Arc::new(Mutex::new(Some(reply)));
    let lost = slot.clone();
    post_core_checked(
        move |provided, _| {
            let Some(reply) = slot.lock().ok().and_then(|mut slot| slot.take()) else {
                return;
            };
            let Some(driver) = provided.all::<Driver>().into_iter().next() else {
                let _ = reply.send(Err(unavailable()));
                return;
            };
            let id = NEXT_SESSION.with(|next| next.replace(next.get() + 1));
            let sink: Rc<dyn Fn(&AgentSessionEvent)> = Rc::new(move |event| {
                on_event(event.to_json());
                if matches!(event, AgentSessionEvent::Ended) {
                    SESSIONS.with(|sessions| sessions.borrow_mut().remove(&id));
                }
            });
            let rt = driver.rt();
            let deps = driver.providers().claude;
            driver.run(
                async move { start_claude_agent_session(&rt, &deps, &options, sink).await },
                move |started| {
                    let _ = reply.send(match started {
                        Ok(session) => {
                            SESSIONS.with(|sessions| sessions.borrow_mut().insert(id, session));
                            Ok(Arc::new(Handle { id }) as Arc<dyn ClaudeSession>)
                        }
                        Err(error) => Err(fault_of(&error)),
                    });
                },
            );
        },
        move || {
            if let Some(reply) = lost.lock().ok().and_then(|mut slot| slot.take()) {
                let _ = reply.send(Err(unavailable()));
            }
        },
    );
    answer.recv().unwrap_or_else(|_| Err(unavailable()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::packages::ai_provider::jsval::string_list;

    fn options(mode: AgentPermissionMode) -> AgentSessionOptions {
        AgentSessionOptions {
            project_root: "/work".to_owned(),
            mode,
            model: String::new(),
            effort: None,
            cli_path: None,
            instructions: String::new(),
            open_documents: vec!["/work/a.docx".to_owned(), "/work/(odd).docx".to_owned()],
            protected_paths: vec!["/secret".to_owned()],
            skill_bundle_dir: None,
            app_mcp: None,
            user_mcp_relays: None,
            session_dir: "/tmp/session".to_owned(),
        }
    }

    fn relay(name: &str, tools: &[&str]) -> AgentMcpRelay {
        AgentMcpRelay {
            name: name.to_owned(),
            url: "http://127.0.0.1:1/mcp".to_owned(),
            token: "t".to_owned(),
            tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        }
    }

    #[test]
    fn launch_args_hold_the_fixed_flags_and_the_session_files() {
        let args = launch_args(&Launch {
            model: " opus ",
            effort: Some("high"),
            mode: AgentPermissionMode::TrustProject,
            settings_file: "/s/claude-settings.json",
            mcp_config_file: Some("/s/claude-mcp.json"),
            skill_bundle_dir: Some("/skills"),
        })
        .unwrap();
        let line = args.join(" ");
        assert!(line.starts_with("-p --input-format stream-json --output-format stream-json"));
        assert!(line.contains(
            "--tools Read,Edit,Write,Glob,Grep,Bash,PowerShell,Skill,WebSearch,WebFetch"
        ));
        assert!(line.contains("--permission-mode auto"));
        assert!(
            line.contains("--settings /s/claude-settings.json --mcp-config /s/claude-mcp.json")
        );
        assert!(line.contains("--plugin-dir /skills --add-dir /skills"));
        assert!(line.contains("--model opus"));
        let manual = launch_args(&Launch {
            model: "",
            effort: None,
            mode: AgentPermissionMode::AskCommands,
            settings_file: "s",
            mcp_config_file: None,
            skill_bundle_dir: None,
        })
        .unwrap();
        assert!(manual.join(" ").contains("--permission-mode default"));
        assert!(!manual.contains(&"--model".to_owned()));
        let bad = launch_args(&Launch {
            model: "--evil",
            effort: None,
            mode: AgentPermissionMode::AskCommands,
            settings_file: "s",
            mcp_config_file: None,
            skill_bundle_dir: None,
        });
        assert!(bad.is_err());
    }

    #[test]
    fn rule_paths_are_the_cli_form_and_refuse_a_pattern_character() {
        assert_eq!(rule_path("/a/b", "linux").as_deref(), Some("//a/b"));
        assert_eq!(rule_path("C:\\a\\b", "win32").as_deref(), Some("//c/a/b"));
        assert_eq!(rule_path("relative", "linux"), None);
        assert_eq!(rule_path("/a/(b)", "linux"), None);
        assert_eq!(rule_path("/a/*", "linux"), None);
        assert_eq!(rule_path("\\\\server\\share", "win32"), None);
    }

    #[test]
    fn settings_deny_protected_and_open_documents() {
        let value = settings(&options(AgentPermissionMode::AskCommands), "linux");
        let text = json_stringify(&value);
        assert!(text.contains("\"blockReadsOutsideWorkingDirectories\":true"));
        assert!(text.contains("Read(//secret)"));
        assert!(text.contains("Write(//secret/**)"));
        assert!(text.contains("Edit(//work/a.docx)"));
        assert!(!text.contains("(odd)"));
        assert!(!text.contains("\"allow\""));
        let mut trusting = options(AgentPermissionMode::TrustProject);
        trusting.user_mcp_relays = Some(vec![relay("notes", &["read"])]);
        assert!(
            json_stringify(&settings(&trusting, "linux")).contains("\"allow\":[\"mcp__notes\"]")
        );
    }

    #[test]
    fn the_mcp_config_names_the_relays_and_the_app_server() {
        let app = AgentAppMcp {
            url: "http://127.0.0.1:9/mcp".to_owned(),
            token: "tok".to_owned(),
        };
        let config = json_stringify(&mcp_config(Some(&app), &[relay("notes", &["read"])]));
        assert!(config.contains("\"notes\""));
        assert!(config.contains("\"seuoffice-editor\""));
        assert!(config.contains("\"Authorization\":\"Bearer tok\""));
    }

    #[test]
    fn actions_are_named_in_the_policys_terms() {
        let input = |entries: Vec<(&str, JsonValue)>| object(entries);
        let read = action(
            "Read",
            Some(&input(vec![("file_path", text("/work/x"))])),
            &[],
        );
        assert_eq!(read.kind, AgentActionKind::Read);
        assert_eq!(read.paths, vec!["/work/x".to_owned()]);
        let command = action(
            "Bash",
            Some(&input(vec![
                ("command", text("ls")),
                ("description", text("list")),
            ])),
            &[],
        );
        assert_eq!(command.kind, AgentActionKind::Command);
        assert_eq!(command.command.as_deref(), Some("ls"));
        assert_eq!(command.reason.as_deref(), Some("list"));
        let fetch = action(
            "WebFetch",
            Some(&input(vec![("url", text("https://Example.com/a"))])),
            &[],
        );
        assert_eq!(fetch.kind, AgentActionKind::WebFetch);
        assert_eq!(fetch.host.as_deref(), Some("example.com"));
        let app = action("mcp__seuoffice-editor__read_document", None, &[]);
        assert_eq!(app.kind, AgentActionKind::AppTool);
        let relayed = action("mcp__notes__read", None, &[relay("notes", &["read"])]);
        assert_eq!(relayed.kind, AgentActionKind::AppTool);
        assert_eq!(action("Mystery", None, &[]).kind, AgentActionKind::Other);
    }

    #[test]
    fn the_init_check_refuses_tools_and_servers_the_app_did_not_give() {
        let relays = [relay("notes", &["read"])];
        let good = object(vec![
            (
                "tools",
                string_list(&[
                    "Read",
                    "Bash",
                    "mcp__seuoffice-editor__open_documents",
                    "mcp__notes__read",
                ]),
            ),
            (
                "mcp_servers",
                JsonValue::Array(vec![
                    object(vec![("name", text("notes"))]),
                    object(vec![("name", text("seuoffice-editor"))]),
                ]),
            ),
        ]);
        assert_eq!(init_problem(&good, &relays), None);
        let extra = object(vec![("tools", string_list(&["Read", "Task"]))]);
        assert!(
            init_problem(&extra, &relays)
                .unwrap()
                .contains("tools the app did not turn on")
        );
        let unlisted = object(vec![("tools", string_list(&["mcp__notes__write"]))]);
        assert!(init_problem(&unlisted, &relays).is_some());
        let foreign = object(vec![
            ("tools", string_list(&["Read"])),
            (
                "mcp_servers",
                JsonValue::Array(vec![object(vec![("name", text("evil"))])]),
            ),
        ]);
        assert!(
            init_problem(&foreign, &relays)
                .unwrap()
                .contains("MCP server")
        );
        assert_eq!(
            init_problem(&object(Vec::new()), &relays).as_deref(),
            Some("Claude Code did not report its tool list")
        );
    }
}
