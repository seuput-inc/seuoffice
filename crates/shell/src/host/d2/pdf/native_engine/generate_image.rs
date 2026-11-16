//! PDF's screen-context adapter to the shared AI image generation service.

use jscompat::{JsObject, JsonValue};
use main_core::packages::ai_provider::jsval::{thrown_message, thrown_to_string};
use main_core::packages::ai_search::media_tools::document_media_roots;
use main_core::slides::ai_ipc::MediaToolOptions;
use shell_contract::{IpcValue, runtime::CancelToken};

pub(crate) type ImageTool =
    fn(&str, &JsonValue, &MediaToolOptions, &CancelToken) -> Result<JsonValue, JsonValue>;

/// The caller supplies the live generator and the core-time status snapshot.
pub(crate) fn generate_image(
    settings_path: &str,
    prompt: &str,
    aspect_ratio: Option<&str>,
    open_path: Option<&str>,
    pasted_dir: &str,
    statuses: JsonValue,
    cancel: &CancelToken,
    tool: ImageTool,
) -> Result<IpcValue, String> {
    generate_with(
        settings_path,
        prompt,
        aspect_ratio,
        open_path,
        pasted_dir,
        cancel,
        statuses,
        tool,
    )
}

fn generate_with(
    settings_path: &str,
    prompt: &str,
    aspect_ratio: Option<&str>,
    open_path: Option<&str>,
    pasted_dir: &str,
    cancel: &CancelToken,
    statuses: JsonValue,
    tool: impl FnOnce(&str, &JsonValue, &MediaToolOptions, &CancelToken) -> Result<JsonValue, JsonValue>,
) -> Result<IpcValue, String> {
    let mut request = JsObject::new();
    request.set("prompt", JsonValue::String(prompt.to_owned()));
    if let Some(aspect_ratio) = aspect_ratio {
        request.set("aspectRatio", JsonValue::String(aspect_ratio.to_owned()));
    }
    let mut context = JsObject::new();
    context.set("path", JsonValue::String("screen".to_owned()));
    context.set("statuses", statuses);
    let options = MediaToolOptions {
        media_roots: document_media_roots(open_path, Some(pasted_dir)),
        context: JsonValue::Object(context),
    };
    tool(settings_path, &JsonValue::Object(request), &options, cancel)
        .map(|value| ipc_value(&value))
        .map_err(|thrown| match thrown_message(&thrown) {
            Some(JsonValue::String(message)) => message,
            _ => thrown_to_string(&thrown).unwrap_or_default(),
        })
}

fn ipc_value(value: &JsonValue) -> IpcValue {
    match value {
        JsonValue::Null => IpcValue::Null,
        JsonValue::Bool(value) => IpcValue::Bool(*value),
        JsonValue::Number(value) => IpcValue::Number(*value),
        JsonValue::String(value) => IpcValue::String(value.clone()),
        JsonValue::Array(values) => IpcValue::Array(values.iter().map(ipc_value).collect()),
        JsonValue::Object(values) => IpcValue::Object(
            values
                .entries()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), ipc_value(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jscompat::{json_value_from_serde, json_value_to_serde};
    use serde_json::json;
    use std::cell::Cell;

    #[test]
    fn request_uses_live_statuses_document_roots_and_original_signal() {
        let calls = Cell::new(0);
        for state in ["signed-out", "ready"] {
            let cancel = CancelToken::default();
            let result = generate_with(
                "/user/ai-settings.json",
                "red kite",
                Some("16:9"),
                Some("/docs/report.pdf"),
                "/staging",
                &cancel,
                {
                    calls.set(calls.get() + 1);
                    json_value_from_serde(&json!({"codex": {"state": state}}))
                },
                |settings, request, options, signal| {
                    assert_eq!(settings, "/user/ai-settings.json");
                    assert_eq!(
                        json_value_to_serde(request),
                        json!({"prompt":"red kite","aspectRatio":"16:9"})
                    );
                    assert_eq!(options.media_roots, ["/docs", "/staging"]);
                    assert_eq!(
                        json_value_to_serde(&options.context),
                        json!({"path":"screen","statuses":{"codex":{"state":state}}})
                    );
                    cancel.cancel();
                    assert!(signal.is_cancelled());
                    Ok(json_value_from_serde(
                        &json!({"url":"file:///generated.png"}),
                    ))
                },
            )
            .unwrap();
            assert_eq!(
                result,
                IpcValue::Object(vec![(
                    "url".into(),
                    IpcValue::String("file:///generated.png".into())
                )])
            );
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn unsaved_document_uses_only_staging_and_propagates_tool_errors() {
        let error = generate_with(
            "settings",
            "",
            None,
            None,
            "/staging",
            &CancelToken::default(),
            json_value_from_serde(&json!({})),
            |_, request, options, _| {
                assert_eq!(json_value_to_serde(request), json!({"prompt":""}));
                assert_eq!(options.media_roots, ["/staging"]);
                Err(json_value_from_serde(&json!({"message":"provider failed"})))
            },
        )
        .unwrap_err();
        assert_eq!(error, "provider failed");
    }
}
