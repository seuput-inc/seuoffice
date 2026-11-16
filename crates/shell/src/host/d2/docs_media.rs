//! Word media analysis through the shared guarded AI transport.

use main_core::packages::ai_search::media_tools::document_media_roots;
use main_core::slides::ai_ipc::MediaToolOptions;
use shell_contract::runtime::CancelToken;
use shell_ipc_d2::docs::hosts::AnalyzeMediaRequest;

pub(super) fn analyze(
    request: AnalyzeMediaRequest,
    cancel: &CancelToken,
) -> Result<serde_json::Value, String> {
    let roots = document_media_roots(request.open_doc.as_deref(), Some(&request.pasted_dir));
    let statuses = crate::host::net::ai::subscription_statuses();
    let options = MediaToolOptions {
        media_roots: roots,
        context: jscompat::json_value_from_serde(&serde_json::json!({
            "path": "screen", "statuses": jscompat::json_value_to_serde(&statuses),
        })),
    };
    let input = jscompat::json_value_from_serde(&serde_json::json!({
        "mediaUrls": request.media_urls, "requirements": request.requirements,
    }));
    crate::host::net::ai::media_tools::analyze_media(
        &request.settings_path,
        &input,
        &options,
        cancel,
    )
    .map(|value| jscompat::json_value_to_serde(&value))
    .map_err(|error| {
        let value = jscompat::json_value_to_serde(&error);
        value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    })
}
