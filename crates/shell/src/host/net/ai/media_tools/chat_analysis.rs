//! Image analysis through the selected chat provider's streaming protocol.

use std::cell::RefCell;
use std::rc::Rc;

use jscompat::{JsObject, JsonValue};
use main_core::packages::ai_provider::media_protocols::{MediaBlob, bytes_to_base64};
use main_core::packages::ai_provider::protocols_shared::StreamCallbacks;
use main_core::packages::ai_provider::stream::{Providers, StreamForProviderOptions, stream_for_provider};
use main_core::packages::ai_provider::task::{JsError, Rt, Signal};

const MAX_BYTES: usize = 20 * 1024 * 1024;
const SYSTEM: &str = "You are shown one or more images from an office document or from the user. Answer the request about them directly and completely, in plain text. You have no tools: do not try to call any.";

pub(super) async fn analyze(
    rt: &Rt, providers: &Providers, provider: &str, config: &JsonValue,
    media: &[MediaBlob], requirements: &str, max_tokens: f64, cancel: &Signal,
) -> Result<String, JsError> {
    let total = media.iter().map(|item| item.bytes.len()).sum::<usize>();
    if total > MAX_BYTES {
        return Err(JsError::error(format!("The images are too large to show to the AI provider ({total} bytes, limit {MAX_BYTES}); analyze fewer or smaller images")));
    }
    let images = media.iter().map(|item| {
        let mut image = JsObject::new();
        image.set("base64", JsonValue::String(bytes_to_base64(&item.bytes)));
        image.set("mime", JsonValue::String(item.mime.clone()));
        JsonValue::Object(image)
    }).collect();
    let mut message = JsObject::new();
    message.set("role", JsonValue::String("user".into()));
    message.set("text", JsonValue::String(requirements.into()));
    message.set("images", JsonValue::Array(images));
    let timeout = rt.timeout_signal(300_000.0);
    let signal = rt.any_signal(&[cancel, &timeout]);
    let answer = Rc::new(RefCell::new(String::new()));
    let output = answer.clone();
    let callbacks = StreamCallbacks {
        on_delta: Rc::new(move |value| if let JsonValue::String(delta) = value { output.borrow_mut().push_str(&delta); }),
        on_tool_call: Rc::new(|_| {}),
        on_reasoning_delta: None, on_stop_reason: None, on_usage: None,
        on_subscription_usage: None, on_input_truncated: None, on_activity: None,
        session_id: None, signal,
    };
    let result = stream_for_provider(rt, providers, provider, config, SYSTEM,
        &[JsonValue::Object(message)], &[], max_tokens, &callbacks, &StreamForProviderOptions::default()).await;
    if timeout.aborted() {
        let mut error = JsError::new("AiRunError", "Image analysis timed out");
        error.props.set("kind", JsonValue::String("timeout".into()));
        return Err(error);
    }
    result?;
    if cancel.aborted() { return Err(JsError::error("Image analysis was cancelled")); }
    let text = answer.borrow().clone();
    if text.trim().is_empty() { return Err(JsError::error("The AI provider returned no analysis")); }
    Ok(text)
}
