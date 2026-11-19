//! The hosts of Settings > Integrations (`shell_ipc_a::integrations`) and of
//! the user skills (`shell_ipc_a::user_skills`).
//!
//! `AgentSkills` is main-core's `cli_agent_skills` and `cli_install` over the
//! machine's home and environment. `SkillsShell` locates the bundled skills.
//! The import host uses main-core for staged installs and zip exports. Template
//! copies use the shared tab state and its editor and project hooks.
//!
//! Where the bundled files are: a packaged shell carries them under the
//! resource folder (`cli/`, `builtin-skills/`); a shell run from a checkout
//! reads the repository, as the Electron build's unpackaged run did. A folder
//! under the resources that exists is what says "packaged", as it does for the
//! renderer bundles (`app_scheme.rs`).

use std::path::PathBuf;
use std::sync::Arc;

use jscompat::{JsObject, JsonValue, json_value_from_serde};
use main_core::packages::ai_provider::user_skills_import::{
    ChangedCheck, GitHubSkillUpdateCheck, InstallImportedSkillsResult, InstalledSkill, SkillImport,
    SkillImportSource, Thrown, remove_quietly,
};
use main_core::packages::cli_agent_skills as skills;
use main_core::packages::cli_install::{
    Env, InstallOptions, InstallOutcome, inspect_cli_link, run_power_shell,
};
use main_core::packages::electron_utils::unsaved_files::{OpenFileState, OpenFilesSourceFailed};
use main_core::shell::user_skills::builtin_dir::{BuiltinSkillsDirOptions, builtin_skills_dir_for};
use main_core::shell::user_skills::template_tabs::TemplateCopy;
use main_core::shell::user_skills::user_skills_service::{
    Aborted, ApplyUpdateOptions, CheckUpdateOptions, Discard, InstallOptions as ImportOptions,
    PrepareEnv,
};
use shell_contract::{Hosts, ViewId};
use shell_ipc_a::integrations::{
    AgentSkills, AgentSkillsHost, AgentTarget, BundledSkill, Ledger, SkillInstallState,
};
use shell_ipc_a::user_skills::{
    OpenedCopy, SkillImporter, SkillImporterHost, SkillsShell, SkillsShellHost, TemplateCopyTabs,
    TemplateCopyTabsHost,
};

use super::now_ms;
use crate::host::HostContext;

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let layout = Layout {
        resources: crate::paths::resources_path(&cx.app),
        repo: repo_root(),
    };
    let agent_skills: AgentSkillsHost = Arc::new(ShellAgentSkills {
        layout: layout.clone(),
    });
    hosts.insert(agent_skills);
    hosts.insert(shell_ipc_a::integrations::factory());

    let importer: SkillImporterHost = Arc::new(ShellSkillImporter);
    hosts.insert(importer);
    let skills_shell: SkillsShellHost = Arc::new(ShellSkills { layout });
    hosts.insert(skills_shell);
    let tabs: TemplateCopyTabsHost = Arc::new(ShellTemplateCopyTabs);
    hosts.insert(tabs);
    hosts.insert(shell_ipc_a::user_skills::factory());
    hosts.insert(shell_ipc_a::user_skills::template_tabs_factory());
}

/// The apps folder, as `app_scheme.rs` finds it: `SEUOFFICE_APPS_DIR`, else
/// the checkout the shell was built from
fn apps_dir() -> PathBuf {
    std::env::var_os("SEUOFFICE_APPS_DIR").map_or_else(
        || PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../apps")),
        PathBuf::from,
    )
}

/// `APPS_ROOT/..` of the Electron build
fn repo_root() -> PathBuf {
    apps_dir().join("..")
}

#[derive(Clone)]
struct Layout {
    resources: Option<PathBuf>,
    repo: PathBuf,
}

impl Layout {
    /// `<resources>/<name>` when the shell is packaged (the folder is there)
    fn packaged(&self, name: &str) -> Option<PathBuf> {
        self.resources
            .as_ref()
            .map(|resources| resources.join(name))
            .filter(|dir| dir.is_dir())
    }

    fn text(path: PathBuf) -> String {
        path.to_string_lossy().into_owned()
    }
}

// ---- Settings > Integrations ----

struct ShellAgentSkills {
    layout: Layout,
}

fn env_of(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn home() -> String {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .to_string_lossy()
        .into_owned()
}

fn target_of(target: skills::AgentTarget) -> AgentTarget {
    AgentTarget {
        id: target.id,
        label: target.label,
        skills_dir: target.skills_dir,
        legacy: target.legacy.unwrap_or(false),
    }
}

fn bundled_of(bundled: &BundledSkill) -> skills::BundledSkill {
    skills::BundledSkill {
        bytes: bundled.bytes.clone(),
        text: bundled.text.clone(),
        version: bundled.version.clone(),
        sha256: bundled.sha256.clone(),
    }
}

impl AgentSkills for ShellAgentSkills {
    fn cli_dir(&self) -> String {
        Layout::text(match self.layout.packaged("cli") {
            Some(cli) => cli,
            None => self.layout.repo.join("packages/cli/bin"),
        })
    }

    fn skill_path(&self) -> String {
        Layout::text(match self.layout.packaged("cli") {
            Some(cli) => cli.join("skills/seuoffice/SKILL.md"),
            None => self.layout.repo.join("skills/seuoffice/SKILL.md"),
        })
    }

    fn cli_package_json(&self) -> String {
        Layout::text(match self.layout.packaged("cli") {
            Some(cli) => cli.join("package.json"),
            None => self.layout.repo.join("packages/cli/package.json"),
        })
    }

    fn is_packaged(&self) -> bool {
        self.layout.packaged("cli").is_some()
    }

    fn resources_path(&self) -> String {
        self.layout
            .resources
            .as_ref()
            .map_or_else(String::new, |dir| dir.to_string_lossy().into_owned())
    }

    fn env(&self, name: &str) -> Option<String> {
        env_of(name)
    }

    fn platform(&self) -> String {
        shell_ipc_a::secrets::PLATFORM.to_owned()
    }

    fn bundled_skill_from(&self, bytes: Vec<u8>) -> BundledSkill {
        let bundled = skills::bundled_skill_from(&bytes);
        BundledSkill {
            bytes: bundled.bytes,
            text: bundled.text,
            version: bundled.version,
            sha256: bundled.sha256,
        }
    }

    fn detect_agents(&self) -> Vec<AgentTarget> {
        skills::detect_agents(&env_of, &home())
            .into_iter()
            .map(target_of)
            .collect()
    }

    fn agent_target(&self, id: &str) -> Option<AgentTarget> {
        skills::agent_target(id, &env_of, &home()).map(target_of)
    }

    fn skill_rows(&self, ledger: &Ledger) -> Vec<AgentTarget> {
        skills::skill_rows(ledger, &env_of, &home())
            .into_iter()
            .map(target_of)
            .collect()
    }

    fn legacy_skill_copies(&self, ledger: &Ledger, only: &str) -> Vec<AgentTarget> {
        skills::legacy_skill_copies(ledger, &env_of, &home(), Some(only))
            .into_iter()
            .map(target_of)
            .collect()
    }

    fn skill_files_written(&self, skills_dir: &str, ledger: &Ledger) -> Vec<String> {
        skills::skill_files_written(skills_dir, ledger)
    }

    fn read_install_state(
        &self,
        skills_dir: &str,
        bundled: &BundledSkill,
        ledger: &Ledger,
    ) -> SkillInstallState {
        let state = skills::read_install_state(skills_dir, &bundled_of(bundled), ledger);
        // the status as the TypeScript spells it: its serde name
        let status = serde_json::to_value(state.status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        SkillInstallState {
            status,
            path: state.path,
            installed_version: state.installed_version,
            older: state.older,
        }
    }

    fn install_skill(
        &self,
        skills_dir: &str,
        bundled: &BundledSkill,
        ledger: &mut Ledger,
    ) -> Result<String, String> {
        skills::install_skill(skills_dir, &bundled_of(bundled), ledger, None, now_ms())
            .map_err(|error| error.message)
    }

    fn uninstall_skill(&self, skills_dir: &str, ledger: &mut Ledger) -> bool {
        match skills::uninstall_skill(skills_dir, ledger) {
            Ok(removed) => removed,
            Err(error) => {
                eprintln!("shell: uninstalling the skill failed: {}", error.message);
                false
            }
        }
    }

    fn build_skill_zip(&self, bundled: &BundledSkill) -> Result<Vec<u8>, String> {
        skills::build_skill_zip(&bundled_of(bundled))
    }

    fn inspect_cli_link(&self, launcher: &str) -> JsonValue {
        let env: Env = &env_of;
        let outcome = inspect_cli_link(&InstallOptions {
            launcher,
            platform: shell_ipc_a::secrets::PLATFORM,
            candidate_dirs: None,
            env,
            run_power_shell: &run_power_shell,
        });
        match outcome.map(|outcome: InstallOutcome| serde_json::to_value(&outcome)) {
            Ok(Ok(value)) => json_value_from_serde(&value),
            // the link could not be read: what a person is told is that it is not usable
            _ => {
                let mut object = JsObject::new();
                object.set("status", JsonValue::String("unwritable".into()));
                JsonValue::Object(object)
            }
        }
    }
}

// ---- user skills ----

struct ShellSkills {
    layout: Layout,
}

impl SkillsShell for ShellSkills {
    fn builtin_skills_dir(&self) -> String {
        let packaged = self.layout.packaged("builtin-skills").is_some();
        let resources = self
            .layout
            .resources
            .as_ref()
            .map_or_else(String::new, |dir| dir.to_string_lossy().into_owned());
        // `app.getAppPath()` of an unpackaged run is the shell package folder
        let app_path = Layout::text(apps_dir().join("shell"));
        builtin_skills_dir_for(&BuiltinSkillsDirOptions {
            is_packaged: packaged,
            resources_path: &resources,
            app_path: &app_path,
        })
    }
}

struct ShellSkillImporter;
struct ShellTemplateCopyTabs;

fn github_get(
    url: &str,
    accept: &str,
    limit: usize,
    aborted: Option<Aborted<'_>>,
) -> Result<Vec<u8>, main_core::packages::ai_provider::user_skills_import::SkillImportError> {
    use main_core::packages::ai_provider::user_skills_import::SkillImportError;
    use std::io::Read;
    let error = |kind, message: String| SkillImportError::new(kind, message);
    let validate = |url: reqwest::Url| -> Result<reqwest::Url, SkillImportError> {
        if url.scheme() != "https"
            || !matches!(
                url.host_str(),
                Some("github.com" | "api.github.com" | "codeload.github.com")
            )
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return Err(error(
                "unsafe",
                format!(
                    "Refused to connect to {}://{}",
                    url.scheme(),
                    url.host_str().unwrap_or("")
                ),
            ));
        }
        Ok(url)
    };
    let mut current = validate(
        reqwest::Url::parse(url)
            .map_err(|_| error("unsafe", format!("Not a web address: {url}")))?,
    )?;
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| error("network", "Could not initialize network client".into()))?;
    for hop in 0..=5 {
        if aborted.is_some_and(|aborted| aborted()) {
            return Err(error("cancelled", "Cancelled".into()));
        }
        let mut response = client
            .get(current.clone())
            .header(reqwest::header::ACCEPT, accept)
            .header(reqwest::header::USER_AGENT, "seuoffice")
            .send()
            .map_err(|_| {
                error(
                    "network",
                    format!("Could not reach {}", current.host_str().unwrap_or("")),
                )
            })?;
        let status = response.status();
        if status.is_redirection() {
            let target = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| error("network", "A redirect without a target".into()))?;
            if hop == 5 {
                return Err(error("network", "Too many redirects".into()));
            }
            current = validate(
                current
                    .join(target)
                    .map_err(|_| error("unsafe", "A redirect to a malformed address".into()))?,
            )?;
            continue;
        }
        if !status.is_success() {
            let code = status.as_u16();
            let rate_limited = code == 429
                || (code == 403
                    && (response
                        .headers()
                        .get("x-ratelimit-remaining")
                        .is_some_and(|value| value == "0")
                        || response.headers().contains_key("retry-after")));
            let kind = if rate_limited {
                "rate-limited"
            } else if [401, 403, 404, 410, 422, 451].contains(&code) {
                "not-found"
            } else {
                "network"
            };
            let message = if rate_limited {
                format!("GitHub is limiting requests (HTTP {code})")
            } else {
                format!("GitHub answered HTTP {code}")
            };
            return Err(error(kind, message));
        }
        if let Some(length) = response
            .content_length()
            .filter(|length| *length > limit as u64)
        {
            return Err(error(
                "too-large",
                format!("The download is {length} bytes (limit {limit})"),
            ));
        }
        let mut body = Vec::new();
        let mut chunk = [0u8; 16384];
        loop {
            if aborted.is_some_and(|aborted| aborted()) {
                return Err(error("cancelled", "Cancelled".into()));
            }
            let count = response
                .read(&mut chunk)
                .map_err(|_| error("network", "The download broke off".into()))?;
            if count == 0 {
                return Ok(body);
            }
            if body.len() + count > limit {
                return Err(error(
                    "too-large",
                    format!("The download is over {limit} bytes"),
                ));
            }
            body.extend_from_slice(&chunk[..count]);
        }
    }
    Err(error("network", "Too many redirects".into()))
}

impl SkillImporter for ShellSkillImporter {
    fn prepare_skill_import(
        &self,
        source: &SkillImportSource,
        env: &PrepareEnv,
        aborted: Option<Aborted<'_>>,
    ) -> Result<SkillImport, Thrown> {
        use main_core::packages::ai_provider::{
            user_skills_archive as archive, user_skills_github as github,
            user_skills_import as import, user_skills_preview as preview,
        };
        let describe = |root: &str, rels: &[String], app: &str| {
            preview::describe_candidates(root, rels, app, None)
        };
        match source {
            SkillImportSource::Folder { folder } => {
                import::prepare_folder_import(folder, &env.app_skills_dir, &env.home_dir, describe)
            }
            SkillImportSource::Text { text } => import::prepare_single_file_import(
                text,
                import::SkillImportOrigin::Text,
                &env.app_skills_dir,
                &env.temp_dir,
                describe,
            ),
            SkillImportSource::SkillFile { file } => import::prepare_skill_file_import(
                file,
                &env.app_skills_dir,
                &env.home_dir,
                &env.temp_dir,
                describe,
            ),
            SkillImportSource::Zip { file } => import::prepare_zip_import(
                file,
                &env.app_skills_dir,
                &env.temp_dir,
                |bytes, root| archive::extract(bytes, root, false, "", false),
                describe,
            ),
            SkillImportSource::Github { address } => {
                let parsed = github::parse_github_skill_address(address).ok_or_else(|| {
                    import::SkillImportError::new(
                        "bad-address",
                        "Not a GitHub address the app accepts",
                    )
                })?;
                let mut get =
                    |url: &str, accept: &str, limit: usize| github_get(url, accept, limit, aborted);
                let resolved = github::resolve_github_skill_address(&parsed, &mut get)?;
                let bytes = github::download_github_archive(
                    &resolved.owner,
                    &resolved.repo,
                    &resolved.commit,
                    None,
                    &mut get,
                )?;
                let origin = import::SkillImportOrigin::Github {
                    repo: format!("{}/{}", resolved.owner, resolved.repo),
                    ref_: resolved.ref_,
                    commit: resolved.commit,
                    path: resolved.path,
                };
                import::prepare_github_archive(
                    origin,
                    resolved.skill.as_deref(),
                    &bytes,
                    &env.app_skills_dir,
                    &env.temp_dir,
                    |bytes, root, path| archive::extract(bytes, root, true, path, true),
                    preview::describe_candidates,
                    aborted,
                )
            }
        }
    }

    fn install_imported_skills(
        &self,
        imported: &SkillImport,
        options: &ImportOptions,
        discard: Discard<'_>,
    ) -> Result<InstallImportedSkillsResult, Thrown> {
        main_core::packages::ai_provider::user_skills_import::install_imported_skills(
            imported,
            &options.app_skills_dir,
            &options.selected,
            &options.on_collision,
            &mut main_core::packages::ai_provider::user_skills_import::CommitSkillInstallOptions {
                now_ms: now_ms(),
                discard_replaced: Some(discard),
                limits: None,
                random: None,
            },
        )
    }

    /// `disposeSkillImport(imported)`: the import's temporary folder goes
    fn dispose_skill_import(&self, imported: &SkillImport) {
        if let Some(dir) = &imported.temp_dir {
            remove_quietly(dir);
        }
    }

    fn check_github_skill_update(
        &self,
        options: &CheckUpdateOptions,
        aborted: Option<Aborted<'_>>,
    ) -> Result<GitHubSkillUpdateCheck, Thrown> {
        use main_core::packages::ai_provider::{
            user_skills_archive as archive, user_skills_github as github,
            user_skills_import as import, user_skills_preview as preview,
        };
        import::check_github_skill_update(
            &options.skill_dir,
            &options.app_skills_dir,
            &options.temp_dir,
            |repo| github::validate_repo(repo).map_err(Into::into),
            |repo, ref_| {
                let address = github::parse_github_skill_address(repo).ok_or_else(|| {
                    import::SkillImportError::new(
                        "not-github",
                        "The source record names no repository",
                    )
                })?;
                let mut get =
                    |url: &str, accept: &str, limit: usize| github_get(url, accept, limit, aborted);
                github::github_ref_commit(&address.owner, &address.repo, ref_, &mut get)
                    .map_err(Into::into)
            },
            |repo, commit| {
                let address = github::parse_github_skill_address(repo).ok_or_else(|| {
                    import::SkillImportError::new(
                        "not-github",
                        "The source record names no repository",
                    )
                })?;
                let mut get =
                    |url: &str, accept: &str, limit: usize| github_get(url, accept, limit, aborted);
                github::download_github_archive(
                    &address.owner,
                    &address.repo,
                    commit,
                    None,
                    &mut get,
                )
                .map_err(Into::into)
            },
            |bytes, root, path| archive::extract(bytes, root, true, path, false),
            |dir, rel, app, ignored| {
                preview::describe_skill_candidate(dir, rel, app, &[ignored.to_owned()])
            },
            aborted,
        )
    }

    fn apply_github_skill_update(
        &self,
        check: &ChangedCheck,
        options: &ApplyUpdateOptions,
        discard: Discard<'_>,
    ) -> Result<InstalledSkill, Thrown> {
        use main_core::packages::ai_provider::user_skills_import::{
            CommitSkillInstallOptions, SkillImportError, SkillInstallPlan, SkillInstallSource,
            commit_skill_install,
        };
        let result = (|| {
            let candidate = check
                .update
                .candidates
                .first()
                .ok_or_else(|| SkillImportError::new("no-skills", "Nothing to update"))?;
            if candidate.problem == Some("too-large") {
                return Err(SkillImportError::new(
                    "too-large",
                    format!("{} is too large", candidate.name),
                )
                .into());
            }
            if candidate.problem.is_some() {
                return Err(SkillImportError::new(
                    "not-installable",
                    format!("{} cannot be installed", candidate.name),
                )
                .into());
            }
            let plan = SkillInstallPlan {
                candidate: candidate.clone(),
                folder_name: jscompat::node_path::host::basename(&options.skill_dir, None),
                replace_dirs: vec![options.skill_dir.clone()],
                source: Some(SkillInstallSource {
                    repo: check.record.repo.clone(),
                    ref_: check.record.ref_.clone(),
                    path: check.record.path.clone(),
                    commit: check.commit.clone(),
                }),
            };
            let mut dirs = commit_skill_install(
                &options.app_skills_dir,
                &[plan],
                &mut CommitSkillInstallOptions {
                    now_ms: now_ms(),
                    discard_replaced: Some(discard),
                    limits: None,
                    random: None,
                },
            )?;
            Ok(InstalledSkill {
                key: candidate.key.clone(),
                name: candidate.name.clone(),
                dir: dirs.remove(0),
                replaced: true,
            })
        })();
        self.dispose_skill_import(&check.update);
        result
    }

    fn export_skill_zip(&self, skill_dir: &str, out_path: &str) -> Result<(), Thrown> {
        main_core::packages::ai_provider::user_skills_import::export_skill_bundle_zip(
            &[skill_dir.to_owned()],
            out_path,
            now_ms(),
        )
    }

    fn export_skill_bundle_zip(&self, skill_dirs: &[String], out_path: &str) -> Result<(), Thrown> {
        main_core::packages::ai_provider::user_skills_import::export_skill_bundle_zip(
            skill_dirs,
            out_path,
            now_ms(),
        )
    }
}

impl TemplateCopyTabs for ShellTemplateCopyTabs {
    fn open_template_copy(
        &self,
        copy: &TemplateCopy,
        requester: Option<ViewId>,
        done: Box<dyn FnOnce(Result<Option<OpenedCopy>, String>)>,
    ) {
        let pending = std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
        let local = pending.clone();
        let copy = copy.clone();
        let started = crate::host::w::on_core_local(move |provided| {
            let Some(done) = local.borrow_mut().take() else {
                return;
            };
            let Some(tabs) = provided
                .all::<shell_ipc_w::tabs::TabsState>()
                .into_iter()
                .next()
            else {
                return done(Ok(None));
            };
            tabs.open_template_copy_owned(
                &copy,
                requester,
                Box::new(move |result| {
                    done(result.map(|opened| {
                        opened.map(|(tab_id, view)| OpenedCopy {
                            tab_id,
                            view,
                            owns_copy: false,
                        })
                    }));
                }),
            );
        });
        if !started && let Some(done) = pending.borrow_mut().take() {
            done(Err("Core thread is unavailable".into()));
        }
    }

    fn opener_file_path(&self, opener: ViewId) -> Option<String> {
        crate::host::w::on_core(move |provided| {
            crate::host::shell_tabs::shell_tabs(provided)
                .and_then(|tabs| tabs.describe_view(opener))
                .and_then(|(_, path)| path)
        })
        .flatten()
    }

    fn open_files(&self, done: Box<dyn FnOnce(Result<Vec<OpenFileState>, OpenFilesSourceFailed>)>) {
        let mut done = Some(done);
        let _ = crate::host::w::on_core_local(|provided| {
            let Some(tabs) = provided
                .all::<shell_ipc_w::tabs::TabsState>()
                .into_iter()
                .next()
            else {
                return;
            };
            let Some(done) = done.take() else { return };
            tabs.open_documents(Box::new(move |result| {
                done(
                    result
                        .map(|documents| {
                            documents
                                .into_iter()
                                .map(|document| OpenFileState {
                                    file_path: document.file_path,
                                    dirty: document.dirty,
                                })
                                .collect()
                        })
                        .map_err(|_| OpenFilesSourceFailed),
                );
            }));
        });
        if let Some(done) = done {
            done(Err(OpenFilesSourceFailed));
        }
    }

    fn focus_tab(&self, tab_id: &str) -> bool {
        let tab_id = tab_id.to_owned();
        crate::host::w::on_core(move |provided| {
            let Some(tabs) = provided
                .all::<shell_ipc_w::tabs::TabsState>()
                .into_iter()
                .next()
            else {
                return false;
            };
            let exists = tabs
                .with_manager(|manager, _| manager.list().iter().any(|tab| tab.id == tab_id))
                .unwrap_or(false);
            if exists {
                tabs.reveal_shell_window();
                let _ = tabs.with_manager(|manager, host| manager.activate_tab(host, &tab_id));
            }
            exists
        })
        .unwrap_or(false)
    }

    fn follow_opener_project(&self, copy: ViewId, opener: ViewId) {
        let _ = crate::host::w::on_core(move |provided| {
            if let Some(tabs) = provided
                .all::<shell_ipc_w::tabs::TabsState>()
                .into_iter()
                .next()
            {
                let opener = main_core::shell::projects::new_documents::NewDocumentOpener::Window {
                    web_contents_id: i64::from(opener.0),
                };
                if let Err(error) = tabs.follow_opener_project(copy, &opener) {
                    eprintln!("Could not link template copy to opener project: {error}");
                }
            }
        });
    }
}
