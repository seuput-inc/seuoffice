//! The agent window's hosts (`shell_ipc_a::agent`): `AgentServices` (the
//! `deps` of `configureAgentWindow`) and `AgentWindowFrame`, and the factory
//! of the port.
//!
//! | method | now |
//! | --- | --- |
//! | `lang`, `theme`, `statuses`, `ai_settings`, `random_uuid` | real |
//! | `list_skills`, `enabled_for_agent`, `open_template_preview` | real: the skills port's `SkillsCore` and `TemplateTabsCore`, through `Provided` on the core thread (the template copy's tab manager is the user skills host's own stub) |
//! | `process_alive`, `exec_file` | real (`std::process`; `exec_file` blocks to its end, bounded by the request's timeout: only the session folders' sweep on a worker asks it) |
//! | `open_documents`, `open_path` | real: the tab manager's open and detached files; the native `OpenFile` route (`docs_tabs::route_path_native`) |
//! | `build_skill_bundle` | real: [`skill_bundle`] (`buildAgentSkillBundle`) |
//! | `has_app_mcp`, `start_app_mcp` | real: one loopback server per session ([`app_mcp::start_agent_server`]) |
//! | `has_user_mcp`, `start_relays` | real: a relay per server of the MCP hub ([`app_mcp::start_agent_relays`]); an absent hub answers no relays |
//! | `run_claude` | real: [`claude`] (`startClaudeAgentSession`) on the AI runtime's driver |
//! | `deck_open`, `deck_text` | real: [`deck`] (the `zip` crate, the PPTX zip gate first) |
//! | `AgentWindowFrame` | real: [`frame`], a window of the window host with the `ShellAgent` bridge |

mod claude;
mod deck;
mod frame;
mod skill_bundle;

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use jscompat::{JsonValue, json_value_to_serde};
use main_core::packages::ai_provider::jsval::{as_str, object, prop, text};
use main_core::shell::agent::agent_session_folders::ExecRequest;
use main_core::shell::agent::agent_sessions::{AgentSessionOptions, AgentSkillBundle};
use main_core::shell::agent::agent_skills::{AgentSkillListEntry, DeckEntry};
use main_core::shell::agent::agent_window::{TemplatePreviewInput, TemplatePreviewOutcome};
use main_core::shell::app_settings::read_app_settings;
use main_core::shell::user_skills::template_tabs::PreviewInput;
use main_core::shell::user_skills::user_skills_service::{AgentEnabledSkill, Failure};
use shell_contract::hosts::Hosts;
use shell_contract::runtime::{AppPaths, PathName};
use shell_contract::{Surface, ViewId};
use shell_ipc_a::agent::{
    AgentServices, AgentServicesHost, AgentWindowFrameHost, AppMcpStarted, ClaudeSession, Fault,
    RelayAsker, RelaysStarted, RunnerFault, SessionRef, SkillHooks,
};
use shell_ipc_a::user_skills::{SkillsCore, TemplateTabsCore};
use shell_ipc_w::tabs::{TabsState, native_open_surface};

use self::deck::Deck;
use self::frame::Frame;
use super::settings::SettingsFile;
use super::{UiLang, subscription_statuses};
use crate::host::HostContext;
use crate::host::app_mcp::{self, AgentMcpEnv};
use crate::host::d2::docs_tabs;
use crate::host::update;
use crate::host::w::on_core;
use crate::paths::ShellPaths;

/// The skills port is not registered
fn no_skills_port() -> String {
    "the skills port is not built".to_owned()
}

/// What a skills method ended with other than its value: its reason
fn reason(failure: Failure) -> String {
    failure.as_fail().reason
}

/// A skill of `userSkills.list()` as the card's target needs it
fn list_entry(skill: &JsonValue) -> AgentSkillListEntry {
    let word = |key: &str| as_str(prop(skill, key)).unwrap_or_default().to_owned();
    AgentSkillListEntry {
        id: word("id"),
        source: word("source"),
        name: word("name"),
        title: word("title"),
        owned: matches!(prop(skill, "owned"), Some(JsonValue::Bool(true))),
        unreadable: prop(skill, "unreadable").cloned(),
        editors: prop(skill, "editors").cloned(),
    }
}

/// An `AgentEnabledSkill` as the skills service lists it
fn enabled_json(skill: &AgentEnabledSkill) -> JsonValue {
    let mut entries = vec![
        ("id", text(&skill.id)),
        ("source", text(&skill.source)),
        ("name", text(&skill.name)),
        ("title", text(&skill.title)),
        ("description", text(&skill.description)),
        ("dir", text(&skill.dir)),
        ("invoke", text(&skill.invoke)),
        ("agentOnly", JsonValue::Bool(skill.agent_only)),
    ];
    if let Some(plugin) = &skill.plugin {
        entries.push(("plugin", text(plugin)));
    }
    object(entries)
}

/// `currentTheme()`: the saved choice, else `system`
fn current_theme() -> String {
    let path = ShellPaths::new().settings_file();
    let settings = read_app_settings(&path.to_string_lossy());
    match settings.get("theme") {
        Some(JsonValue::String(theme)) if theme == "light" || theme == "dark" => theme.clone(),
        _ => "system".to_owned(),
    }
}

/// `process.kill(pid, 0)` did not throw, or threw `EPERM`
fn process_alive(pid: f64) -> bool {
    if !(1.0..=f64::from(u32::MAX)).contains(&pid) {
        return false;
    }
    let pid = pid as u32;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let listed = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .creation_flags(0x0800_0000)
            .output();
        listed.is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")))
    }
    #[cfg(not(windows))]
    {
        match Command::new("kill").args(["-0", &pid.to_string()]).output() {
            Ok(out) => {
                out.status.success()
                    || String::from_utf8_lossy(&out.stderr)
                        .to_lowercase()
                        .contains("not permitted")
            }
            Err(_) => false,
        }
    }
}

/// `execFile(file, args, { cwd, timeout, windowsHide: true, maxBuffer })` to
/// its end: what the program wrote to stdout, empty where it failed (it could
/// not start, exited with an error, ran past the timeout or wrote more than
/// `maxBuffer`)
fn exec_file(request: &ExecRequest) -> String {
    let mut command = Command::new(&request.file);
    command
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = command.spawn() else {
        return String::new();
    };
    let Some(mut stdout) = child.stdout.take() else {
        return String::new();
    };
    let max = request.max_buffer as usize;
    let overflow = Arc::new(AtomicBool::new(false));
    let flag = overflow.clone();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    out.extend_from_slice(&chunk[..n]);
                    if out.len() > max {
                        flag.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        }
        out
    });
    // `timeout: 0` is no timeout
    let deadline = (request.timeout_ms > 0)
        .then(|| Instant::now() + Duration::from_millis(u64::from(request.timeout_ms)));
    let succeeded = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {
                let late = deadline.is_some_and(|deadline| Instant::now() >= deadline);
                if late || overflow.load(Ordering::Acquire) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break false,
        }
    };
    let out = reader.join().unwrap_or_default();
    if succeeded && !overflow.load(Ordering::Acquire) {
        String::from_utf8_lossy(&out).into_owned()
    } else {
        String::new()
    }
}

struct Services {
    lang: UiLang,
    settings: SettingsFile,
    /// what a session's app MCP server is made from; `None` where the shell has no editors to offer
    mcp: Option<Arc<AgentMcpEnv>>,
    deck: Deck,
}

impl AgentServices for Services {
    fn lang(&self) -> String {
        self.lang.code()
    }

    fn theme(&self) -> String {
        current_theme()
    }

    fn statuses(&self) -> JsonValue {
        subscription_statuses()
    }

    /// `readStoredAiSettings()`
    fn ai_settings(&self) -> JsonValue {
        self.settings.stored().unwrap_or_else(|error| {
            eprintln!("shell: reading the AI settings: {error}");
            JsonValue::Object(jscompat::JsObject::new())
        })
    }

    /// `[...tabManager.openFilePaths(), ...detachedFilePaths()]`
    fn open_documents(&self) -> Vec<String> {
        on_core(|provided| {
            provided
                .all::<TabsState>()
                .first()
                .map(|tabs| tabs.all_file_paths())
        })
        .flatten()
        .unwrap_or_default()
    }

    /// `routeDocumentPath(path)`: false where the file is not there or no editor takes its type;
    /// the open itself goes through the native `OpenFile` route, which settles later
    fn open_path(&self, path: &str) -> bool {
        if !std::path::Path::new(path).exists() {
            return false;
        }
        let routable = native_open_surface(path) != Surface::Shell;
        let shown = path.to_owned();
        docs_tabs::route_path_native(
            path.to_owned(),
            Box::new(move |opened| {
                if let Err(error) = opened {
                    eprintln!("shell: the agent window could not open {shown}: {error}");
                }
            }),
        );
        routable
    }

    fn list_skills(&self) -> Result<Vec<AgentSkillListEntry>, String> {
        let listed = on_core(|provided| {
            provided
                .all::<SkillsCore>()
                .first()
                .map(|core| core.list().map_err(reason))
        })
        .flatten()
        .ok_or_else(no_skills_port)?;
        Ok(listed?.iter().map(list_entry).collect())
    }

    fn enabled_for_agent(&self, folder: &str) -> Result<Vec<JsonValue>, String> {
        let folder = folder.to_owned();
        let enabled = on_core(move |provided| {
            provided
                .all::<SkillsCore>()
                .first()
                .map(|core| core.enabled_for_agent(Some(&folder)).map_err(reason))
        })
        .flatten()
        .ok_or_else(no_skills_port)?;
        Ok(enabled?.iter().map(enabled_json).collect())
    }

    fn enabled_for_agent_task(
        &self,
        folder: &str,
    ) -> Result<Box<dyn FnOnce() -> Result<Vec<JsonValue>, String> + Send>, String> {
        let folder = folder.to_owned();
        let task = on_core(move |provided| {
            provided
                .all::<SkillsCore>()
                .first()
                .map(|core| core.enabled_for_agent_task(Some(folder)))
        })
        .flatten()
        .ok_or_else(no_skills_port)?;
        Ok(Box::new(move || {
            let enabled = task().map_err(reason)?;
            Ok(enabled.iter().map(enabled_json).collect())
        }))
    }

    fn build_skill_bundle(
        &self,
        skills: &[JsonValue],
        dir: &str,
    ) -> Result<Option<AgentSkillBundle>, Fault> {
        skill_bundle::build_agent_skill_bundle(skills, dir).map(Some)
    }

    fn open_template_preview(
        &self,
        owner: ViewId,
        input: &TemplatePreviewInput,
        done: Box<dyn FnOnce(Result<TemplatePreviewOutcome, String>)>,
    ) {
        let proposal = match serde_json::to_value(&input.proposal) {
            Ok(proposal) => proposal,
            Err(error) => return done(Err(error.to_string())),
        };
        let preview = PreviewInput {
            bytes: input.bytes.clone(),
            file_name: input.file_name.clone(),
            title: input.title.clone(),
            tab_title: input.tab_title.clone(),
            text: input.text.clone(),
            original_redacted: input.original_redacted,
            proposal,
            draft: input.draft.as_ref().map(json_value_to_serde),
        };
        let pending = std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
        let local = pending.clone();
        let started = crate::host::w::on_core_local(move |provided| {
            let Some(done) = local.borrow_mut().take() else {
                return;
            };
            let Some(core) = provided.all::<TemplateTabsCore>().into_iter().next() else {
                return done(Err(no_skills_port()));
            };
            core.open_preview(
                owner,
                &preview,
                Box::new(move |opened| {
                    done(match opened {
                        Ok((tab_id, preview_id)) => {
                            Ok(TemplatePreviewOutcome::Opened { tab_id, preview_id })
                        }
                        Err(Failure::Fail(fail)) => Ok(TemplatePreviewOutcome::Refused {
                            reason: fail.reason,
                        }),
                        Err(thrown) => Err(reason(thrown)),
                    })
                }),
            );
        });
        if !started && let Some(done) = pending.borrow_mut().take() {
            done(Err("Core services are not available".into()));
        }
    }

    fn has_app_mcp(&self) -> bool {
        self.mcp.is_some()
    }

    /// `startAgentMcpServer({ ...mcp, skillTools }, projectRoot)`: the session's own server on a
    /// thread of its own, its skill tools bound to `hooks`
    fn start_app_mcp(
        &self,
        project_root: &str,
        _: &SessionRef,
        hooks: Arc<dyn SkillHooks>,
    ) -> Result<AppMcpStarted, Fault> {
        let env = self
            .mcp
            .clone()
            .ok_or_else(|| Fault::new("the MCP runtime is not configured"))?;
        let started =
            app_mcp::start_agent_server(env, project_root.to_owned(), hooks).map_err(Fault::new)?;
        Ok(AppMcpStarted {
            url: started.url,
            token: started.token,
            stop: started.stop,
        })
    }

    /// `userMcp` is always given (the hub of the person's servers); an absent hub answers no relays
    fn has_user_mcp(&self) -> bool {
        true
    }

    fn start_relays(
        &self,
        session: &SessionRef,
        ask: Arc<dyn RelayAsker>,
    ) -> Result<RelaysStarted, Fault> {
        if !app_mcp::has_hub() {
            return Ok(RelaysStarted {
                relays: Vec::new(),
                keep_results_inside: Vec::new(),
                stop: Box::new(|| {}),
            });
        }
        app_mcp::start_agent_relays(update::app_version().to_owned(), &session.session_id, ask)
            .map_err(Fault::new)
    }

    fn run_claude(
        &self,
        options: &AgentSessionOptions,
        on_event: Arc<dyn Fn(JsonValue) + Send + Sync>,
    ) -> Result<Arc<dyn ClaudeSession>, RunnerFault> {
        claude::run(options, on_event)
    }

    fn deck_open(&self, bytes: &[u8]) -> Result<Vec<DeckEntry>, String> {
        self.deck.open(bytes)
    }

    fn deck_text(&self, index: usize) -> Result<String, String> {
        self.deck.text(index)
    }

    fn process_alive(&self, pid: f64) -> bool {
        process_alive(pid)
    }

    fn exec_file(&self, request: &ExecRequest) -> String {
        exec_file(request)
    }
}

/// Inserts the two hosts and the factory of the agent window's port
pub fn install(hosts: &mut Hosts, lang: &UiLang, cx: &HostContext) {
    let paths = ShellPaths::new();
    let services: AgentServicesHost = Arc::new(Services {
        lang: lang.clone(),
        settings: SettingsFile::new(paths.path(PathName::UserData), paths.path(PathName::Home)),
        mcp: AgentMcpEnv::new(cx, hosts).map(Arc::new),
        deck: Deck::default(),
    });
    hosts.insert(services);
    let frame: AgentWindowFrameHost = Arc::new(Frame::default());
    hosts.insert(frame);
    hosts.insert(shell_ipc_a::agent::factory());
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    fn request(file: &str, args: &[&str], timeout_ms: u32, max_buffer: u32) -> ExecRequest {
        ExecRequest {
            file: file.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            cwd: None,
            timeout_ms,
            max_buffer,
        }
    }

    #[cfg(unix)]
    #[test]
    fn exec_file_answers_stdout_and_nothing_where_it_failed() {
        assert_eq!(
            exec_file(&request("/bin/echo", &["hello"], 5000, 1024)),
            "hello\n"
        );
        // an error exit, a program that is not there, output over the buffer, the timeout
        assert_eq!(exec_file(&request("/usr/bin/false", &[], 5000, 1024)), "");
        assert_eq!(exec_file(&request("/no/such/program", &[], 5000, 1024)), "");
        assert_eq!(
            exec_file(&request("/bin/echo", &["0123456789"], 5000, 4)),
            ""
        );
        assert_eq!(exec_file(&request("/bin/sleep", &["5"], 100, 1024)), "");
    }

    #[cfg(unix)]
    #[test]
    fn a_process_is_alive_while_it_runs() {
        assert!(process_alive(f64::from(std::process::id())));
        assert!(!process_alive(0.0));
        assert!(!process_alive(f64::NAN));
    }
}
