use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{json, Value};
use tauri::Webview;
use tauri_runtime_cef::{DevToolsProtocol, WebviewCefExt};

use crate::host::contents::cdp;

#[derive(Default)]
struct Contexts {
    by_frame: HashMap<String, (i64, String)>,
}

impl Contexts {
    fn event(&mut self, method: &str, params: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(params) else { return };
        match method {
            "Runtime.executionContextCreated" => {
                let Some(context) = value.get("context") else { return };
                let Some(aux) = context.get("auxData") else { return };
                if aux.get("isDefault").and_then(Value::as_bool) != Some(true) { return; }
                let (Some(frame), Some(id), Some(unique)) = (
                    aux.get("frameId").and_then(Value::as_str),
                    context.get("id").and_then(Value::as_i64),
                    context.get("uniqueId").and_then(Value::as_str),
                ) else { return };
                self.by_frame.insert(frame.to_owned(), (id, unique.to_owned()));
            }
            "Runtime.executionContextDestroyed" => {
                if let Some(id) = value.get("executionContextId").and_then(Value::as_i64) {
                    self.by_frame.retain(|_, (context_id, _)| *context_id != id);
                }
            }
            "Runtime.executionContextsCleared" => self.by_frame.clear(),
            _ => {}
        }
    }
}

static CONTEXTS: LazyLock<Mutex<HashMap<String, Arc<Mutex<Contexts>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn forget(label: &str) {
    if let Ok(mut contexts) = CONTEXTS.lock() { contexts.remove(label); }
}

fn observer(webview: &Webview) -> Result<Arc<Mutex<Contexts>>, String> {
    let mut all = CONTEXTS.lock().map_err(|_| "HWP context registry unavailable")?;
    if let Some(contexts) = all.get(webview.label()) { return Ok(contexts.clone()); }
    let contexts = Arc::new(Mutex::new(Contexts::default()));
    let listener = contexts.clone();
    webview.on_dev_tools_protocol(move |event| {
        if let DevToolsProtocol::Event { method, params } = event
            && let Ok(mut contexts) = listener.lock()
        {
            contexts.event(&method, &params);
        }
    }).map_err(|error| error.to_string())?;
    cdp(webview, "Runtime.enable", json!({}))?;
    all.insert(webview.label().to_owned(), contexts.clone());
    Ok(contexts)
}

fn studio_frame(tree: &Value) -> Option<&str> {
    let frame = tree.get("frame")?;
    if frame.get("url").and_then(Value::as_str).is_some_and(|url| url.contains("/studio/")) {
        return frame.get("id").and_then(Value::as_str);
    }
    tree.get("childFrames").and_then(Value::as_array)
        .and_then(|children| children.iter().find_map(studio_frame))
}

pub(super) fn editor_takes(webview: &Webview, key: &str) -> bool {
    let result = (|| -> Result<bool, String> {
        let contexts = observer(webview)?;
        let tree = cdp(webview, "Page.getFrameTree", json!({}))?;
        let frame = studio_frame(&tree["frameTree"]).ok_or("HWP studio frame not found")?;
        let unique = contexts.lock().map_err(|_| "HWP contexts unavailable")?
            .by_frame.get(frame).map(|(_, unique)| unique.clone())
            .ok_or("HWP studio execution context not found")?;
        let encoded = serde_json::to_string(key).map_err(|error| error.to_string())?;
        let expression = format!("typeof window.__ieumHancomKey === 'function' && window.__ieumHancomKey({encoded}) === true");
        let answer = cdp(webview, "Runtime.evaluate", json!({
            "expression": expression, "uniqueContextId": unique, "returnByValue": true,
        }))?;
        Ok(answer.get("exceptionDetails").is_none() && answer["result"]["value"] == true)
    })();
    result.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_studio_child_and_forgets_destroyed_contexts() {
        let tree = json!({"frame":{"id":"root","url":"app://hwp/"},"childFrames":[{"frame":{"id":"studio","url":"app://hwp/studio/index.html"}}]});
        assert_eq!(studio_frame(&tree), Some("studio"));
        let mut contexts = Contexts::default();
        contexts.event("Runtime.executionContextCreated", br#"{"context":{"id":7,"uniqueId":"unique","auxData":{"isDefault":true,"frameId":"studio"}}}"#);
        assert_eq!(contexts.by_frame.get("studio").map(|value| value.1.as_str()), Some("unique"));
        contexts.event("Runtime.executionContextDestroyed", br#"{"executionContextId":7}"#);
        assert!(!contexts.by_frame.contains_key("studio"));
    }

    #[test]
    fn ignores_non_default_context_and_clears_on_navigation() {
        let mut contexts = Contexts::default();
        contexts.event("Runtime.executionContextCreated", br#"{"context":{"id":3,"uniqueId":"isolated","auxData":{"isDefault":false,"frameId":"studio"}}}"#);
        assert!(contexts.by_frame.is_empty());
        contexts.event("Runtime.executionContextCreated", br#"{"context":{"id":4,"uniqueId":"main","auxData":{"isDefault":true,"frameId":"studio"}}}"#);
        contexts.event("Runtime.executionContextsCleared", b"{}");
        assert!(contexts.by_frame.is_empty());
    }
}
