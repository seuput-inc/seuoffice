//! Production hosts for MCP, knowledge bases, Drive, skills and generation.
//!
//! Each port receives its concrete host and factory here. Renderer operations
//! retain the native window/document identity supplied by the shared hosts;
//! native context authentication is a separate requirement for IPC admission.

use main_core::packages::i18n::{Lang, install_lang};
use main_core::shell::app_settings::read_app_settings;
use shell_contract::Hosts;

use super::super::HostContext;
use crate::paths::ShellPaths;

mod drive;
pub(crate) mod generate;
mod generate_caption;
mod generate_outline;
mod generate_window;
mod kb;
mod mcp;
mod schema;
mod skills;

/// `currentLang()` of the shell entry: `SEUOFFICE_LANG`, else the language
/// recorded in app-settings.json, else the system's. Read at each call, so a
/// language the person switched is seen at once.
fn current_lang() -> Lang {
    if let Ok(forced) = std::env::var("SEUOFFICE_LANG") {
        return main_core::packages::i18n::normalize_lang(Some(&forced));
    }
    let path = ShellPaths::new().settings_file();
    let settings = read_app_settings(&path.to_string_lossy());
    install_lang(settings.get("language"), system_locale().as_deref())
}

/// `app.getLocale()`, as far as the environment says. A GUI launch on macOS
/// carries none of these, so the default there is English until a locale
/// source (the `sys-locale` crate) is added.
fn system_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// `Date.now()`
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

pub fn install(hosts: &mut Hosts, cx: &HostContext) {
    mcp::install(hosts, cx);
    kb::install(hosts, cx);
    drive::install(hosts, cx);
    skills::install(hosts, cx);
    generate::install(hosts, cx);
}
