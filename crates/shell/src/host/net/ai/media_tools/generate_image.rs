//! Image generation using the configured provider and the shared AI runtime.

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jscompat::{JsObject, JsonValue, json_parse};
use main_core::packages::ai_provider::image_backend::AiImageBackend;
use main_core::packages::ai_provider::jsval::js_string_checked;
use main_core::packages::ai_provider::legacy_settings::normalize_ai_settings;
use main_core::packages::ai_provider::media_protocols::{GenerateImageInput, generate_image_with_provider};
use main_core::packages::ai_provider::task::JsError;
use main_core::packages::ai_search::headless_ledger::{self, AcquireError};
use main_core::packages::ai_search::tool_routing::AiToolCallPath;
use main_core::packages::ai_search::tool_routing::{ToolRoutingState, image_generation_unavailable_message, resolve_image_route};
use main_core::packages::electron_utils::generated_images::store_generated_image;
use main_core::slides::ai_ipc::MediaToolOptions;
use shell_contract::runtime::CancelToken;
use shell_ipc_a::app_ai_image::CodexImage;

use super::{NEXT, RUNS, context, failure, field, provider_config, provider_failure, refs, text};
use crate::host::w::post_core;

const MODEL_NOTE: &str = "The `model` option is no longer supported and was ignored: special-purpose image models (background removal, upscaling, outpainting, text removal) are gone, and the image was generated with the configured image backend.";
const CODEX_NO_REFERENCES: &str = "Reference images are not supported when images are generated through the Codex subscription. Call again without referenceImageUrls, or select an image provider key under Settings > AI > Advanced to edit images.";

fn answer(op: &JsonValue, image: CodexImage) -> JsonValue {
    let url = match image {
        CodexImage::Url(url) => url,
        CodexImage::Bytes { bytes, mime } => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64);
            match store_generated_image(&std::env::temp_dir().to_string_lossy(), now, Some(&bytes), Some(&mime)) {
                Ok(url) => url,
                Err(error) => return failure(error.message(), None),
            }
        }
    };
    let mut result = JsObject::new();
    result.set("url", JsonValue::String(url));
    if field(op, "model").is_some_and(JsonValue::truthy) { result.set("note", JsonValue::String(MODEL_NOTE.into())); }
    JsonValue::Object(result)
}

pub(crate) fn generate_image(settings_path: &str, op: &JsonValue, options: &MediaToolOptions, cancel: &CancelToken) -> Result<JsonValue, JsonValue> {
    let prompt = match field(op, "prompt") {
        None | Some(JsonValue::Null) => String::new(),
        Some(value) => js_string_checked(value).map_err(|error| JsonValue::String(error.0))?,
    };
    let prompt = jscompat::trim(&prompt).to_owned();
    if prompt.is_empty() { return Ok(failure("prompt must not be empty", None)); }
    let stored = std::fs::read_to_string(settings_path).ok().and_then(|s| json_parse(&s).ok());
    let settings = normalize_ai_settings(super::super::codec(), stored.as_ref(), None);
    let context = context(&options.context);
    let mut routing = ToolRoutingState::default();
    routing.image_backend.set_codex_image_generation_available(super::super::codex_image_available());
    routing.codex_generator_registered = true;
    let route = resolve_image_route(&routing, &settings, &context);
    let (provider, config) = match route {
        AiImageBackend::Key { provider, config } => (provider.to_owned(), provider_config(&config)),
        AiImageBackend::None => return Ok(failure(image_generation_unavailable_message(&routing, &settings, &context), Some("unavailable"))),
        AiImageBackend::Codex => {
            let has_references = match field(op, "referenceImageUrls") {
                Some(JsonValue::Array(items)) => !items.is_empty(),
                Some(JsonValue::String(items)) => !items.is_empty(),
                _ => false,
            };
            if has_references { return Ok(failure(CODEX_NO_REFERENCES, None)); }
            let turn = if context.path == AiToolCallPath::Headless {
                match context.data_dir.as_deref().filter(|dir| !dir.is_empty()) {
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
                }
            } else { None };
            let request = super::super::codex_image::CodexImageRequest {
                prompt,
                aspect_ratio: text(field(op, "aspectRatio")).map(str::to_owned),
                image_size: text(field(op, "imageSize")).map(str::to_owned),
                transparent: matches!(field(op, "transparentBackground"), Some(JsonValue::Bool(true))),
            };
            let result = super::super::codex_image::generate(&request, cancel);
            drop(turn);
            return Ok(match result { Ok(image) => answer(op, image), Err(error) => provider_failure(&error) });
        }
    };
    let references = match field(op, "referenceImageUrls") {
        None | Some(JsonValue::Null) => Vec::new(),
        Some(JsonValue::Array(items)) => {
            let mut refs = Vec::with_capacity(items.len());
            for item in items {
                refs.push(js_string_checked(item).map_err(|error| JsonValue::String(error.0))?);
            }
            refs
        }
        _ => return Ok(failure("referenceImageUrls must be an array", None)),
    };
    let references = match refs::load(&references, &options.media_roots, cancel, &super::super::fetch::media_download) {
        Ok(references) => references,
        Err(error) => return Ok(failure(error, None)),
    };
    let input = GenerateImageInput {
        prompt,
        aspect_ratio: text(field(op, "aspectRatio")).map(str::to_owned),
        references: Some(references),
        transparent: Some(matches!(field(op, "transparentBackground"), Some(JsonValue::Bool(true)))),
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
        let providers = driver.providers();
        RUNS.with(|runs| { runs.borrow_mut().insert(id, (driver.clone(), signal.clone())); });
        driver.run(async move {
            generate_image_with_provider(&rt, &providers.fx, &provider, &config, &input, Some(&signal), &shell_ipc_a::app_ai_image::registrable_domain).await
        }, move |result| {
            RUNS.with(|runs| { runs.borrow_mut().remove(&id); });
            let _ = tx.send(result);
        });
    });
    let deadline = Instant::now() + Duration::from_secs(610);
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(image)) => return Ok(answer(op, CodexImage::Bytes { bytes: image.bytes, mime: image.mime })),
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
                return Ok(failure(if cancel.is_cancelled() { "This operation was aborted" } else { "Image generation timed out" }, None));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_prompt_returns_tool_error_before_provider_selection() {
        let mut op = JsObject::new();
        op.set("prompt", JsonValue::String("  \n ".into()));
        let options = MediaToolOptions { media_roots: vec![], context: JsonValue::Null };
        let result = generate_image("", &JsonValue::Object(op), &options, &CancelToken::default()).unwrap();
        assert_eq!(field(&result, "error"), Some(&JsonValue::String("prompt must not be empty".into())));
    }

    #[test]
    fn returned_url_preserves_the_ignored_model_note() {
        let mut op = JsObject::new();
        op.set("model", JsonValue::String("old-model".into()));
        let result = answer(&JsonValue::Object(op), CodexImage::Url("https://example.test/image".into()));
        assert_eq!(field(&result, "url"), Some(&JsonValue::String("https://example.test/image".into())));
        assert_eq!(field(&result, "note"), Some(&JsonValue::String(MODEL_NOTE.into())));
    }
}
