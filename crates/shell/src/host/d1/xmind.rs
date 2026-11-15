//! XMind opening through the original browser-compatible engine.

use shell_ipc_d1::mindmap::{OpenOutcome, OpenStatus, XmindEngine};

pub(super) struct Engine;

impl XmindEngine for Engine {
    fn open_xmind(&self, bytes: &[u8]) -> Result<OpenOutcome, String> {
        let input = serde_json::json!({ "bytesBase64": jscompat::base64(bytes) });
        let result = seu_host::call("mindmap.open", &input)
            .map_err(|error| format!("{}: {}", error.name, error.message))?;
        let status = result.get("status").and_then(|v| v.as_str())
            .and_then(OpenStatus::parse)
            .ok_or_else(|| "XMind engine returned an invalid status".to_owned())?;
        let detail = match result.get("detail") {
            None => None,
            Some(value) => Some(value.as_str().ok_or("XMind engine returned an invalid detail")?.to_owned()),
        };
        if status != OpenStatus::Ok && detail.is_none() {
            return Err("XMind engine returned no failure detail".to_owned());
        }
        Ok(OpenOutcome { status, detail })
    }
}
