mod new_document;
pub(crate) mod platform;
pub(crate) use new_document::{
    write_new_document_existing_document, write_new_document_owned,
    write_new_document_publication_owned,
};

use std::sync::{Arc, LazyLock, Mutex};

use main_core::packages::project_store::{LegacyDocumentChat, native_store::NativeProjectStore};
use serde_json::Value;
use shell_contract::Hosts;
use shell_contract::ids::ViewId;
use shell_contract::runtime::{AppPaths, PathName};
use shell_ipc_w::projects::{LegacyProjectStore, LegacyProjectStoreHost};

use super::{HostContext, w::on_core};
use crate::paths::ShellPaths;

static STORE: LazyLock<Arc<Mutex<NativeProjectStore>>> = LazyLock::new(|| {
    Arc::new(Mutex::new(NativeProjectStore::new(
        ShellPaths::new().path(PathName::UserData),
    )))
});

pub(super) fn store() -> &'static Arc<Mutex<NativeProjectStore>> {
    &STORE
}

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    hosts.insert(shell_ipc_w::projects::ProjectCleanupShutdown(
        cx.cleanup_shutdown.clone(),
    ));
    let host: LegacyProjectStoreHost = Arc::new(StoreHost(STORE.clone()));
    hosts.insert(host);
    hosts.insert(platform::install(cx));
}

struct StoreHost(Arc<Mutex<NativeProjectStore>>);

fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("{name} must be a string"))
}

impl LegacyProjectStore for StoreHost {
    fn legacy_chat_for(&self, path: &str) -> Result<Option<LegacyDocumentChat>, String> {
        self.0
            .lock()
            .map_err(|error| error.to_string())?
            .legacy_chat_for(path)?
            .map(|value| serde_json::from_value(value).map_err(|error| error.to_string()))
            .transpose()
    }

    fn ensure_default_project(&self) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|error| error.to_string())?
            .ensure_default_project()
            .map(|_| ())
    }

    fn resolve_chat_for_file(&self, path: &Value) -> Result<Value, String> {
        self.0
            .lock()
            .map_err(|error| error.to_string())?
            .resolve_chat_for_file(string(path, "path")?)
    }

    fn lookup_chat_for_file(&self, path: &Value) -> Result<Value, String> {
        self.0
            .lock()
            .map_err(|error| error.to_string())?
            .lookup_chat_for_file(string(path, "path")?)
    }

    fn append_chat_message(
        &self,
        project: &Value,
        chat: &Value,
        message: &Value,
    ) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|error| error.to_string())?
            .append_chat_message(
                string(project, "projectId")?,
                string(chat, "chatId")?,
                message,
            )
    }

    fn load_chat(&self, project: &Value, chat: &Value, limit: &Value) -> Result<Value, String> {
        let limit = limit.as_f64().unwrap_or(f64::NAN);
        self.0
            .lock()
            .map_err(|error| error.to_string())?
            .load_chat(
                string(project, "projectId")?,
                string(chat, "chatId")?,
                limit,
            )
            .map(Value::Array)
    }

    fn rebind_chat_to_file(
        &self,
        project: &Value,
        chat: &Value,
        path: &Value,
    ) -> Result<Value, String> {
        self.0
            .lock()
            .map_err(|error| error.to_string())?
            .rebind_chat_to_file(
                string(project, "projectId")?,
                string(chat, "chatId")?,
                string(path, "path")?,
            )
    }

    fn rebind_chat(&self, project: &Value, chat: &Value, new_chat: &Value) -> Result<(), String> {
        let project = string(project, "projectId")?;
        self.0
            .lock()
            .map_err(|error| error.to_string())?
            .rebind_chat(
                project,
                string(chat, "chatId")?,
                project,
                string(new_chat, "chatId")?,
            )
    }

    fn sheets_session_path(&self, view: ViewId, session: &Value) -> Option<String> {
        let session = session.as_str()?.to_owned();
        on_core(move |_| {
            shell_ipc_d1::sheets::editors::views()?
                .external_link_session(view, &session)
                .map(|(path, _)| path)
        })
        .flatten()
    }

    fn headless(&self) -> bool {
        false
    }
}
