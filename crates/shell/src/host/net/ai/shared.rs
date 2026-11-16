//! `AiCoreHost` and `SlidesAiHost`: the two hosts that share what the AI
//! libraries of `@seuoffice/ai-search` answer. The tool capabilities, the
//! search backends, the page reader and the remote image download are real;
//! the media tools live in [`super::media_tools`].

use jscompat::JsonValue;
use main_core::packages::ai_provider::settings_secrets::AiSecretCodec;
use main_core::packages::ai_search::search_tools::{SearchAnswer, SearchOptions};
use main_core::packages::ai_search::tool_capabilities::ai_tool_capabilities;
use main_core::packages::ai_search::tool_routing::{
    AiToolCallContext, AiToolCallPath, ToolRoutingState,
};
use main_core::packages::i18n::Lang;
use main_core::slides::ai_ipc::{ImageDecoder, MediaToolOptions};
use shell_contract::runtime::CancelToken;
use shell_ipc_a::ai_core::{AiCoreHost, RemoteImage};
use shell_ipc_a::slides_ai::SlidesAiHost;

use super::{UiLang, codec, codex_image_available, fetch, read_url, search, subscription_statuses};
use super::native_image::Decoder;

/// `{ path, statuses?, dataDir? }`
fn context_of(value: &JsonValue) -> AiToolCallContext {
    let field = |key: &str| match value {
        JsonValue::Object(object) => object.get(key),
        _ => None,
    };
    AiToolCallContext {
        path: match field("path") {
            Some(JsonValue::String(path)) if path == "headless" => AiToolCallPath::Headless,
            _ => AiToolCallPath::Screen,
        },
        statuses: field("statuses").cloned(),
        data_dir: match field("dataDir") {
            Some(JsonValue::String(dir)) => Some(dir.clone()),
            _ => None,
        },
    }
}

/// `aiToolCapabilities(settings, context)` over the registered native generator
/// and the subscription service's current availability.
fn tool_capabilities(settings: &JsonValue, context: &JsonValue) -> JsonValue {
    let mut state = ToolRoutingState::default();
    state
        .image_backend
        .set_codex_image_generation_available(codex_image_available());
    state.codex_generator_registered = true;
    match ai_tool_capabilities(&state, Some(settings), &context_of(context), &|name| {
        std::env::var(name).ok()
    }) {
        Ok(capabilities) => capabilities,
        Err(message) => {
            // `getProviderAdapter` threw for an id the registry lacks: the host has no way to throw
            eprintln!("shell: aiToolCapabilities: {message}");
            JsonValue::Object(jscompat::JsObject::new())
        }
    }
}

/// `fetchRemoteImage(url)` with the temp folder the generated images live in
fn remote_image(url: &str) -> Option<RemoteImage> {
    fetch::remote_image(url, &std::env::temp_dir().to_string_lossy())
}

pub struct AiCore {
    pub lang: UiLang,
}

impl AiCoreHost for AiCore {
    fn ui_lang(&self) -> Lang {
        self.lang.get()
    }

    fn subscription_statuses(&self) -> JsonValue {
        subscription_statuses()
    }

    /// The shell has no `--headless-export` entry
    fn is_headless_mode(&self) -> bool {
        false
    }

    fn ai_tool_capabilities(&self, settings: &JsonValue, context: &JsonValue) -> JsonValue {
        tool_capabilities(settings, context)
    }

    fn web_search(&self, query: &str, max_results: f64, options: &SearchOptions) -> SearchAnswer {
        search::web_search(query, max_results, options)
    }

    fn image_search(&self, query: &str, max_results: f64, options: &SearchOptions) -> SearchAnswer {
        search::image_search(query, max_results, options)
    }

    fn read_url(
        &self,
        settings_path: &str,
        url: &str,
        offset: f64,
        signal: &CancelToken,
    ) -> Result<JsonValue, JsonValue> {
        Ok(read_url::read_url(settings_path, url, offset, signal))
    }

    fn fetch_remote_image(&self, url: &str) -> Option<RemoteImage> {
        remote_image(url)
    }
}

pub struct Slides {
    lang: UiLang,
    decoder: Decoder,
}

impl Slides {
    pub fn new(lang: UiLang) -> Slides {
        Slides {
            lang,
            decoder: Decoder,
        }
    }
}

impl SlidesAiHost for Slides {
    fn ui_lang(&self) -> Lang {
        self.lang.get()
    }

    fn secret_codec(&self) -> Option<&dyn AiSecretCodec> {
        codec()
    }

    fn shared_subscription_statuses(&self) -> JsonValue {
        subscription_statuses()
    }

    fn ai_tool_capabilities(&self, settings: &JsonValue, context: &JsonValue) -> JsonValue {
        tool_capabilities(settings, context)
    }

    fn generate_image(
        &self,
        settings: &str,
        operation: &JsonValue,
        options: &MediaToolOptions,
        signal: &CancelToken,
    ) -> Result<JsonValue, JsonValue> {
        super::media_tools::generate_image::generate_image(settings, operation, options, signal)
    }

    fn analyze_media(
        &self,
        settings: &str,
        operation: &JsonValue,
        options: &MediaToolOptions,
        signal: &CancelToken,
    ) -> Result<JsonValue, JsonValue> {
        super::media_tools::analyze_media(settings, operation, options, signal)
    }

    fn fetch_remote_image(&self, url: &str) -> Option<Vec<u8>> {
        remote_image(url).map(|image| image.bytes)
    }

    fn image_decoder(&self) -> &dyn ImageDecoder {
        &self.decoder
    }
}
