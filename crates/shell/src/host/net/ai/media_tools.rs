//! Media analysis over the configured provider and the shell's guarded transport.

mod chat_analysis;
mod refs;
pub(crate) mod generate_image;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jscompat::{JsObject, JsonValue, json_parse};
use main_core::packages::ai_provider::jsval::js_string_checked;
use main_core::packages::ai_provider::legacy_settings::normalize_ai_settings;
use main_core::packages::ai_provider::media_protocols::{AiMediaProviderConfig, AnalyzeMediaInput, analyze_media_with_provider};
use main_core::packages::ai_provider::providers::max_output_tokens_of;
use main_core::packages::ai_provider::run_error::ai_stream_error_fields_of;
use main_core::packages::ai_provider::task::{JsError, Signal};
use main_core::packages::ai_search::headless_ledger::{self, AcquireError};
use main_core::packages::ai_search::tool_routing::{AiImageAnalysisRoute, AiToolCallContext, AiToolCallPath, AiVideoAnalysisRoute, resolve_image_analysis_route, resolve_video_analysis_route};
use main_core::slides::ai_ipc::MediaToolOptions;
use shell_contract::runtime::CancelToken;
use shell_ipc_a::runtime::Driver;

use crate::host::w::post_core;

thread_local! {
    static RUNS: RefCell<HashMap<u64, (Driver, Signal)>> = RefCell::new(HashMap::new());
}
static NEXT: AtomicU64 = AtomicU64::new(1);

enum Route {
    Key { provider: String, config: AiMediaProviderConfig },
    Chat { provider: String, config: JsonValue, max_tokens: f64 },
}

fn field<'a>(value: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    match value { JsonValue::Object(object) => object.get(key), _ => None }
}
fn text(value: Option<&JsonValue>) -> Option<&str> {
    match value { Some(JsonValue::String(value)) => Some(value), _ => None }
}
fn context(value: &JsonValue) -> AiToolCallContext {
    AiToolCallContext {
        path: if text(field(value, "path")) == Some("headless") { AiToolCallPath::Headless } else { AiToolCallPath::Screen },
        statuses: field(value, "statuses").cloned(),
        data_dir: text(field(value, "dataDir")).map(str::to_owned),
    }
}
fn provider_config(value: &JsonValue) -> AiMediaProviderConfig {
    AiMediaProviderConfig {
        api_key: text(field(value, "apiKey")).unwrap_or("").into(),
        base_url: text(field(value, "baseUrl")).map(str::to_owned),
        image_model: text(field(value, "imageModel")).unwrap_or("").into(),
        analysis_model: text(field(value, "analysisModel")).unwrap_or("").into(),
    }
}
fn failure(message: impl Into<String>, code: Option<&str>) -> JsonValue {
    let mut value = JsObject::new();
    value.set("error", JsonValue::String(message.into()));
    if let Some(code) = code { value.set("errorCode", JsonValue::String(code.into())); }
    JsonValue::Object(value)
}
fn provider_failure(error: &JsError) -> JsonValue {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64);
    let fields = ai_stream_error_fields_of(error, now);
    let mut value = JsObject::new();
    value.set("error", JsonValue::String(error.message.clone()));
    if let Some(code) = fields.get("errorCode") { value.set("errorCode", code.clone()); }
    if let Some(resets) = fields.get("errorResetsAt") { value.set("resetsAt", resets.clone()); }
    JsonValue::Object(value)
}

pub(crate) fn analyze_media(settings_path: &str, op: &JsonValue, options: &MediaToolOptions, cancel: &CancelToken) -> Result<JsonValue, JsonValue> {
    let references = match field(op, "mediaUrls") {
        Some(JsonValue::Array(items)) => {
            let mut refs = Vec::with_capacity(items.len());
            for item in items {
                let reference = js_string_checked(item).map_err(|error| JsonValue::String(error.0))?;
                if !reference.is_empty() { refs.push(reference); }
            }
            refs
        }
        None | Some(JsonValue::Null) => Vec::new(),
        _ => return Err(JsonValue::String("mediaUrls.map is not a function".into())),
    };
    let requirements = match field(op, "requirements") {
        None | Some(JsonValue::Null) => String::new(),
        Some(value) => js_string_checked(value).map_err(|error| JsonValue::String(error.0))?,
    };
    let requirements = jscompat::trim(&requirements).to_owned();
    if references.is_empty() { return Ok(failure("mediaUrls must not be empty", None)); }
    if requirements.is_empty() { return Ok(failure("requirements must not be empty", None)); }
    let media = match refs::load(&references, &options.media_roots, cancel, &super::fetch::media_download) {
        Ok(media) => media,
        Err(error) => return Ok(failure(error, None)),
    };
    let stored = std::fs::read_to_string(settings_path).ok().and_then(|s| json_parse(&s).ok());
    let settings = normalize_ai_settings(super::codec(), stored.as_ref(), None);
    let context = context(&options.context);
    let mut subscription_dir = None;
    let route = if media.iter().any(|blob| !blob.mime.starts_with("image/")) {
        match resolve_video_analysis_route(&settings) {
            AiVideoAnalysisRoute::Key(route) => Route::Key { provider: route.provider.into(), config: provider_config(&route.config) },
            AiVideoAnalysisRoute::None { message } => return Ok(failure(message, Some("unavailable"))),
        }
    } else {
        match resolve_image_analysis_route(&settings, &context) {
            Err(error) => return Ok(failure(error, None)),
            Ok(AiImageAnalysisRoute::None { message }) => return Ok(failure(message, Some("unavailable"))),
            Ok(AiImageAnalysisRoute::Key(route)) => Route::Key { provider: route.provider.into(), config: provider_config(&route.config) },
            Ok(AiImageAnalysisRoute::Chat { provider, billing }) => {
                if billing == "subscription" && context.path == AiToolCallPath::Headless {
                    subscription_dir = context.data_dir.clone();
                }
                let config = field(&settings, "providers").and_then(|providers| field(providers, &provider)).cloned().unwrap_or(JsonValue::Null);
                Route::Chat { provider, config, max_tokens: max_output_tokens_of(Some(&settings)) }
            }
        }
    };
    let turn = match subscription_dir.as_deref() {
        Some(dir) => {
            let token = cancel.clone();
            match headless_ledger::acquire(std::path::Path::new(dir), Arc::new(move || token.is_cancelled())) {
                Ok(turn) => Some(turn),
                Err(AcquireError::Cancelled) => return Ok(failure("This operation was aborted", None)),
                Err(AcquireError::Timeout) => return Ok(failure("Timed out waiting for another subscription request from the command line or an integration to finish", Some("timeout"))),
                Err(AcquireError::Io(error)) => return Ok(failure(error, None)),
            }
        }
        None => None,
    };
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel(1);
    post_core(move |provided, _| {
        let Some(subscription) = provided.all::<shell_ipc_a::subscription::SubscriptionShared>().into_iter().next() else {
            let _ = tx.send(Err(JsError::error("AI runtime is unavailable")));
            return;
        };
        let driver = subscription.driver();
        let rt = driver.rt();
        let signal = rt.controller();
        let mut providers = driver.providers();
        if let Some(connection) = subscription.chatgpt_connection() {
            providers.chatgpt.connection = Some(Some(connection));
        }
        RUNS.with(|runs| { runs.borrow_mut().insert(id, (driver.clone(), signal.clone())); });
        driver.run(async move {
            match route {
                Route::Key { provider, config } => analyze_media_with_provider(&rt, &providers.fx, &provider, &config, &AnalyzeMediaInput { media, requirements }, Some(&signal)).await,
                Route::Chat { provider, config, max_tokens } => chat_analysis::analyze(&rt, &providers, &provider, &config, &media, &requirements, max_tokens, &signal).await,
            }
        }, move |result| {
            drop(turn);
            RUNS.with(|runs| { runs.borrow_mut().remove(&id); });
            let _ = tx.send(result);
        });
    });
    let deadline = Instant::now() + Duration::from_secs(310);
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(answer)) => {
                let mut value = JsObject::new();
                value.set("text", JsonValue::String(answer));
                return Ok(JsonValue::Object(value));
            }
            Ok(Err(error)) => return Ok(provider_failure(&error)),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(failure("AI runtime is unavailable", None)),
            Err(mpsc::RecvTimeoutError::Timeout) if cancel.is_cancelled() || Instant::now() >= deadline => {
                post_core(move |_, _| {
                    let run = RUNS.with(|runs| runs.borrow().get(&id).cloned());
                    if let Some((driver, signal)) = run {
                        signal.abort(Some(JsError::abort()));
                        driver.run(async {}, |_| {});
                    }
                });
                return Ok(failure(if cancel.is_cancelled() { "This operation was aborted" } else { "Image analysis timed out" }, None));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_media_and_requirements_report_validation_errors_without_a_provider() {
        let options = MediaToolOptions {
            media_roots: Vec::new(),
            context: JsonValue::Object(JsObject::new()),
        };
        let cancel = CancelToken::default();
        let mut op = JsObject::new();
        op.set("mediaUrls", JsonValue::Array(Vec::new()));
        op.set("requirements", JsonValue::String("inspect".into()));
        let result = analyze_media("missing-synthetic-settings.json", &JsonValue::Object(op), &options, &cancel).unwrap();
        assert_eq!(text(field(&result, "error")), Some("mediaUrls must not be empty"));

        let mut op = JsObject::new();
        op.set("mediaUrls", JsonValue::Array(vec![JsonValue::String("data:image/png;base64,YQ==".into())]));
        op.set("requirements", JsonValue::String("  ".into()));
        let result = analyze_media("missing-synthetic-settings.json", &JsonValue::Object(op), &options, &cancel).unwrap();
        assert_eq!(text(field(&result, "error")), Some("requirements must not be empty"));
    }
}
