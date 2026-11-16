//! Batch A's hosts of the AI ports (host/net/ai): everything the shared AI
//! channels, the per-app AI channels, the slides AI, the free connections, the
//! subscription logins and the entry's AI preferences reach beyond main-core
//! and the shared hosts, and the factories of those ports.
//!
//! What the shell can do is real; what a port needs beyond it comes from the
//! hosts the shell inserts.
//!
//! | trait | now |
//! | --- | --- |
//! | `ai_core::AiCoreHost` | real: `ui_lang` (W's one answer), `subscription_statuses` and `ai_tool_capabilities` (main-core over the subscription service's state), `is_headless_mode` (the shell has no headless export), `web_search`, `image_search` and `read_url` ([`shared`] over reqwest and the SSRF guard), `fetch_remote_image` (main-core's SSRF-guarded download over reqwest) |
//! | `ai_core::PendingAiDocs` | real: one shared accepted-initialization store |
//! | `ai_naming::NamingContext` | real (`NamingChat` is not inserted: the port drives the AI runtime itself) |
//! | `app_ai_image::AppAiImageHost` | real: statuses and the Codex route state. Stub: `codex_generate_image` (`createCodexImageCliGenerator` is not in main-core) |
//! | `slides_ai::SlidesAiHost` | real: `ui_lang`, `secret_codec`, statuses, `ai_tool_capabilities`, `fetch_remote_image`, `image_decoder` (the `image` crate). Stub: `generate_image`, `analyze_media` (`@seuoffice/ai-search` media tools) |
//! | `free_ai::FreeAiHost` | real: settings read and write (the AI settings file, the codec; a write lists the key providers whose key changed), `window_state`, `window_command`, `net` and `open_router` (reqwest, a loopback server) |
//! | `subscription::SubscriptionShell` | real: `ui_lang`, `signature_valid` (ring), `login_window_text` and `chat_gpt_sign_in_pages` ([`subscription_text`]) |
//! | `shell_ai_prefs::ShellUi` | real: `document_locale` (from W's one UI language), `show_home_tab` (the shell window raised, the Home tab opened) |
//! | `outbound::OutboundShell` | real: `lang` |
//! | `agent::AgentServices`, `AgentWindowFrame` | see [`agent`] |
//!
//! `UiLanguage` is W's: the language every trait of this file answers is what
//! W's host in [`Hosts`] answers, so the day W's real one replaces its stub
//! these follow it.

mod agent;
pub(crate) mod codex_image;
pub(crate) mod fetch;
mod free;
mod html_text;
mod jwt;
pub(crate) mod media_tools;
mod native_image;
mod parallel_mcp;
mod read_url;
mod search;
mod settings;
mod shared;
mod subscription_text;

use std::sync::{Arc, OnceLock};

use jscompat::{JsObject, JsonValue};
use main_core::packages::ai_provider::settings_secrets::AiSecretCodec;
use main_core::packages::ai_provider::subscription_ipc::shared_subscription_statuses;
use main_core::packages::ai_provider::task::JsError;
use main_core::packages::i18n::{Lang, document_locale_for, normalize_lang};
use shell_contract::hosts::Hosts;
use shell_contract::ids::ViewId;
use shell_contract::runtime::{AppPaths, CancelToken, PathName, Views};
use shell_ipc_a::ai_core::{PendingAiDocs, PendingAiDocsHost};
use shell_ipc_a::ai_naming::{NamingContext, NamingContextHost};
use shell_ipc_a::app_ai_image::{
    AppAiImageHost, AppAiImageHostHandle, CodexImage, CodexImageRequest, CodexImageRoute,
};
use shell_ipc_a::outbound::{OutboundShell, OutboundShellHost};
use shell_ipc_a::shell_ai_prefs::{ShellUi, ShellUiHost};
use shell_ipc_a::subscription::{SubscriptionShared, SubscriptionShell, SubscriptionShellHost};
use shell_ipc_w::editor_files::UiLanguageHost;
use tauri::{AppHandle, Manager};

use super::super::HostContext;
use super::super::secrets::CODEC;
use crate::views::ViewTable;

/// The language of the UI: W's `UiLanguage`, taken once from [`Hosts`] (W's
/// hosts are installed before these). `ko` where the shell has none.
#[derive(Clone)]
struct UiLang(Option<UiLanguageHost>);

impl UiLang {
    fn get(&self) -> Lang {
        self.0.as_ref().map_or(Lang::Ko, |host| host.lang())
    }

    fn code(&self) -> String {
        self.get().as_str().to_owned()
    }
}

/// The shell's codec for the AI settings, once `host::secrets` has installed it
fn codec() -> Option<&'static dyn AiSecretCodec> {
    CODEC.get().map(|codec| &**codec as &dyn AiSecretCodec)
}

/// `sharedSubscriptionStatuses()`: the subscription service's state lives on
/// the core thread. `{}` before the subscription port registered.
pub(crate) fn subscription_statuses() -> JsonValue {
    super::super::w::on_core(|provided| {
        provided
            .all::<SubscriptionShared>()
            .first()
            .map(|shared| shared_subscription_statuses(&shared.ipc))
    })
    .flatten()
    .unwrap_or_else(|| JsonValue::Object(JsObject::new()))
}

/// `codexImageGenerationAvailable()`, from the subscription service's state
fn codex_image_available() -> bool {
    super::super::w::on_core(|provided| {
        provided
            .all::<SubscriptionShared>()
            .first()
            .map(|shared| shared.image.borrow().codex_image_generation_available())
    })
    .flatten()
    .unwrap_or(false)
}

/// `pendingAiDocContents`: the one instance of the process. What carries out
/// `queueDocsAiContent` for the tabs (W's `TabHooks::queue_docs_ai_content`)
/// writes into this, or the new docs tab never finds its content.
static PENDING_DOCS: OnceLock<PendingAiDocsHost> = OnceLock::new();

pub(crate) fn prepare_ai_docs_seed(
    view: ViewId,
    seed: shell_ipc_a::ai_core::AcceptedAiDoc,
) -> Result<shell_ipc_a::ai_core::PreparedAiSeed, String> {
    PENDING_DOCS
        .get()
        .ok_or("The pending document service is unavailable")?
        .prepare_accepted(view, seed)
}

/// The Tauri window of a view's window
fn window_of(app: &AppHandle, views: &ViewTable, view: ViewId) -> Option<tauri::Window> {
    let window = Views::sender_of(views, view)?.window;
    app.get_window(&views.label_of_window(window)?)
}

/// `NamingContext`
struct Naming {
    lang: UiLang,
}

impl NamingContext for Naming {
    fn subscription_statuses(&self) -> JsonValue {
        subscription_statuses()
    }

    fn ui_lang(&self) -> String {
        self.lang.code()
    }
}

/// `OutboundShell`
struct Outbound {
    lang: UiLang,
}

impl OutboundShell for Outbound {
    fn lang(&self) -> Lang {
        self.lang.get()
    }
}

/// `AppAiImageHost`
struct AppImage;

impl AppAiImageHost for AppImage {
    fn shared_subscription_statuses(&self) -> JsonValue {
        subscription_statuses()
    }

    fn codex_image_route(&self) -> CodexImageRoute {
        CodexImageRoute {
            available: codex_image_available(),
            registered: true,
        }
    }

    fn codex_generate_image(
        &self,
        request: &CodexImageRequest,
        signal: &CancelToken,
    ) -> Result<CodexImage, JsError> {
        codex_image::generate(
            &codex_image::CodexImageRequest {
                prompt: request.prompt.clone(),
                aspect_ratio: request.aspect_ratio.clone(),
                image_size: None,
                transparent: false,
            },
            signal,
        )
    }
}

/// `SubscriptionShell`
struct Subscription {
    lang: UiLang,
}

impl SubscriptionShell for Subscription {
    fn ui_lang(&self) -> String {
        self.lang.code()
    }

    fn login_window_text(&self, lang: &str, provider: &str) -> JsonValue {
        // `{ title?, guide?, closing? }`: a provider the strings do not know keeps the program's own text
        subscription_text::login_window_text(normalize_lang(Some(lang)), provider)
            .unwrap_or_else(|| JsonValue::Object(JsObject::new()))
    }

    fn chat_gpt_sign_in_pages(&self, lang: &str) -> JsonValue {
        subscription_text::chat_gpt_sign_in_pages(normalize_lang(Some(lang)))
    }

    fn signature_valid(
        &self,
        alg: &str,
        data: &[u8],
        signature: &[u8],
        jwk: &JsonValue,
    ) -> Result<bool, String> {
        jwt::signature_valid(alg, data, signature, jwk)
    }
}

/// `ShellUi`
struct Ui {
    lang: UiLang,
}

impl ShellUi for Ui {
    fn document_locale(&self) -> String {
        document_locale_for(Some(self.lang.get().as_str()))
    }

    /// `win.restore()`, `win.show()`, `win.focus()`, `tabManager.openHomeTab()`: the
    /// shell window comes back (made first when it was closed), then the Home tab
    /// opens or is activated. Runs where the tabs live, at once on the core thread
    /// so the settings event the caller sends next finds the Home view.
    fn show_home_tab(&self) {
        let _ = crate::host::w::on_core(|provided| {
            if let Some(tabs) = crate::host::shell_tabs::shell_tabs(provided) {
                tabs.reveal_shell_window();
                tabs.open_home_tab();
            }
        });
    }
}

pub fn install(hosts: &mut Hosts, cx: &HostContext) {
    codex_image::configure(crate::paths::ShellPaths::new().path(PathName::UserData));
    let lang = UiLang(hosts.get::<UiLanguageHost>().cloned());

    let pending: PendingAiDocsHost = Arc::new(PendingAiDocs::default());
    let _ = PENDING_DOCS.set(pending.clone());
    hosts.insert(pending);

    let core: shell_ipc_a::ai_core::AiCoreHostHandle =
        Arc::new(shared::AiCore { lang: lang.clone() });
    hosts.insert(core);
    let slides: shell_ipc_a::slides_ai::SlidesAiHostHandle =
        Arc::new(shared::Slides::new(lang.clone()));
    hosts.insert(slides);
    let naming: NamingContextHost = Arc::new(Naming { lang: lang.clone() });
    hosts.insert(naming);
    let outbound: OutboundShellHost = Arc::new(Outbound { lang: lang.clone() });
    hosts.insert(outbound);
    let image: AppAiImageHostHandle = Arc::new(AppImage);
    hosts.insert(image);
    let subscription: SubscriptionShellHost = Arc::new(Subscription { lang: lang.clone() });
    hosts.insert(subscription);
    let ui: ShellUiHost = Arc::new(Ui { lang: lang.clone() });
    hosts.insert(ui);
    let free: shell_ipc_a::free_ai::FreeAiHostHandle = Arc::new(free::FreeAi::new(
        lang.clone(),
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
    ));
    hosts.insert(free);

    hosts.insert(shell_ipc_a::shell_ai_prefs::factory());
    hosts.insert(shell_ipc_a::outbound::factory());
    hosts.insert(shell_ipc_a::ai_core::factory());
    hosts.insert(shell_ipc_a::ai_core::app_ai_factory());
    hosts.insert(shell_ipc_a::ai_naming::factory());
    hosts.insert(shell_ipc_a::app_ai_image::factory());
    hosts.insert(shell_ipc_a::slides_ai::factory());
    hosts.insert(shell_ipc_a::slides_ai::proposal_factory());
    hosts.insert(shell_ipc_a::free_ai::factory());
    hosts.insert(shell_ipc_a::subscription::factory());
    agent::install(hosts, &lang, cx);
}
