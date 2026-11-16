use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use main_core::packages::ai_provider::settings_secrets::AiSecretCodec;
use main_core::shell::file_index::decider::{DecisionCallOptions, JevDocument, evaluate};
use main_core::shell::home_api::FileSearchSettings;
use shell_contract::Hosts;
use shell_contract::runtime::Thread;
use shell_ipc_w::home_files::Home;
use shell_ipc_w::home_settings::{HomeApp, HomeAppHost, McpSettings, McpStatus};
use shell_ipc_w::tabs::TabsState;
use tauri::Manager;

use super::super::{HostContext, app_mcp::AppMcp, w::on_core};
use super::decision::DecisionTransport;

struct App {
    version: String,
    resources: Option<PathBuf>,
    ui: Arc<dyn Thread>,
    mcp: Arc<AppMcp>,
}

impl HomeApp for App {
    fn version(&self) -> String {
        self.version.clone()
    }
    fn is_packaged(&self) -> bool {
        !cfg!(debug_assertions)
    }
    fn locale(&self) -> String {
        super::super::menu::first_accept_language(&self.ui).unwrap_or_else(|| "en-US".to_owned())
    }
    fn os_release(&self) -> Result<String, String> {
        sysinfo::System::kernel_version()
            .ok_or_else(|| "operating system release unavailable".to_owned())
    }
    fn now_ms(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_millis() as f64
    }
    fn third_party_notices_path(&self) -> PathBuf {
        if self.is_packaged() {
            self.resources
                .as_ref()
                .expect("application resources unavailable")
                .join("THIRD-PARTY-NOTICES.txt")
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../apps/shell/build/THIRD-PARTY-NOTICES.txt")
        }
    }
    fn open_help_owned(
        &self,
        topic: Option<String>,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let mut done = Some(done);
        let accepted = super::super::w::on_core_local(|provided| {
            let Some(done) = done.take() else {
                return;
            };
            let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                return done(Err("The tab manager is unavailable".into()));
            };
            tabs.open_help_owned(
                topic,
                origin,
                cancel,
                pending,
                Box::new(move |result| {
                    done(result.map(|_| ()).map_err(|error| error.0));
                }),
            );
        });
        if !accepted && let Some(done) = done {
            done(Err("Help requires the registered core thread".into()));
        }
    }
    fn auto_update_check_changed(&self, _on: bool) {
        // The updater reads the persisted setting each time it checks.
    }
    fn delimited_import_limit_changed(&self, bytes: f64) {
        super::super::d1::shells::set_delimited_import_limit(bytes);
    }
    fn home_kinds_changed(&self) {
        super::super::menu::home_kinds_changed();
    }
    fn begin_save_dir_change(&self) -> Result<(), String> {
        super::super::document_assets::begin_vault_settings_change()
    }
    fn end_save_dir_change(&self) -> Result<(), String> {
        super::super::document_assets::end_vault_settings_change()
    }
    fn folder_watchers_changed(
        &self,
        done: Box<dyn FnOnce(Result<(), shell_contract::IpcError>) + Send>,
    ) {
        let pending = Arc::new(std::sync::Mutex::new(Some(done)));
        let on_core_pending = pending.clone();
        let scheduled = on_core(move |provided| {
            let done = on_core_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let Some(done) = done else { return };
            match provided.all::<Home>().into_iter().next() {
                Some(home) => home.ensure_folder_watchers(done),
                None => done(Err(shell_contract::IpcError {
                    name: Some("Error".into()),
                    message: "home services unavailable".into(),
                })),
            }
        });
        if scheduled.is_none() {
            let done = pending.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(done) = done {
                done(Err(shell_contract::IpcError {
                    name: Some("Error".into()),
                    message: "core services unavailable".into(),
                }));
            }
        }
    }
    fn secrets(&self) -> Option<&dyn AiSecretCodec> {
        super::super::secrets::CODEC
            .get()
            .map(|codec| codec.as_ref() as &dyn AiSecretCodec)
    }
    fn probe_decision(&self, settings: &FileSearchSettings) -> Result<(), String> {
        let options = DecisionCallOptions {
            endpoint: settings.endpoint,
            key: settings.keys.get(settings.endpoint).to_owned(),
            custom_base_url: settings.custom_base_url.clone(),
            custom_model: settings.custom_model.clone(),
            cloudflare_account_id: settings.cloudflare_account_id.clone(),
            cloudflare_model: settings.cloudflare_model.clone(),
        };
        let docs = vec![
            JevDocument {
                title: "a.md".into(),
                heading: String::new(),
                text: "The connection test document.".into(),
            },
            JevDocument {
                title: "b.md".into(),
                heading: String::new(),
                text: "An unrelated note.".into(),
            },
        ];
        let mut transport = DecisionTransport::new()?;
        evaluate("connection test", &docs, &options, &mut transport)
            .map(|_| ())
            .map_err(|error| error.0)
    }
    fn mcp_status(&self) -> McpStatus {
        self.mcp.status()
    }
    fn mcp_apply(&self, settings: McpSettings) -> Result<McpStatus, String> {
        self.mcp.apply(settings)
    }
    fn mcp_logs(&self) -> Vec<String> {
        self.mcp.logs()
    }
    fn mcp_clear_logs(&self) {
        self.mcp.clear_logs();
    }
    fn mcp_reveal_log_file(&self) {
        self.mcp.reveal_log_file();
    }
    fn mcp_log_file(&self) -> Option<PathBuf> {
        self.mcp.log_file()
    }
}

pub(crate) fn open_help_from_menu(topic: Option<String>) {
    use shell_contract::ipc::NativeEditorCommand;
    let cancel = shell_contract::runtime::CancelToken::default();
    let operation = match super::super::native_commands::issue(
        NativeEditorCommand::OpenHelp {
            topic: topic.clone(),
        },
        cancel,
    ) {
        Ok(operation) => operation,
        Err(error) => {
            eprintln!("Could not open help: {error}");
            return;
        }
    };
    let unavailable = operation.clone();
    super::super::w::post_core_checked(
        move |provided, _| {
            let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                operation.finish();
                eprintln!("Could not open help: the tab manager is unavailable");
                return;
            };
            let completed = operation.clone();
            tabs.open_help_native(
                topic,
                operation,
                Box::new(move |result| {
                    completed.finish();
                    if let Err(error) = result {
                        eprintln!("Could not open help: {}", error.0);
                    }
                }),
            );
        },
        move || {
            unavailable.finish();
            eprintln!("Could not open help: the core thread is unavailable");
        },
    );
}

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let mcp = hosts
        .get::<Arc<AppMcp>>()
        .expect("app MCP runtime unavailable")
        .clone();
    let settings_path = crate::paths::ShellPaths::new().settings_file();
    let settings =
        main_core::shell::app_settings::read_app_settings(&settings_path.to_string_lossy());
    super::super::d1::shells::set_delimited_import_limit(
        main_core::shell::csv_import_limit::csv_import_limit_mb(&settings) * 1024.0 * 1024.0,
    );
    let host: HomeAppHost = Arc::new(App {
        version: cx.app.package_info().version.to_string(),
        resources: crate::paths::resources_path(&cx.app),
        ui: cx.ui.clone(),
        mcp,
    });
    hosts.insert(host);
}
