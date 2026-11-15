use std::sync::{Arc, LazyLock, Mutex};

use main_core::diagram::network_guard::{RequestLog, new_request_log, screen_request};

static SESSION: LazyLock<Result<(tauri_runtime_cef::NativeNetworkSession, Arc<NetworkPolicy>), String>> = LazyLock::new(|| {
        let diagram_url = std::env::var("DIAGRAM_RENDERER_URL").ok();
        let electron_url = std::env::var("ELECTRON_RENDERER_URL").ok();
        let origins = main_core::diagram::network_guard::dev_server_origins(
            &[None, diagram_url.as_deref(), electron_url.as_deref()], !cfg!(debug_assertions));
        let policy = Arc::new(NetworkPolicy::new(origins));
        let handler = policy.clone();
        let session = tauri_runtime_cef::NativeNetworkSession::persistent("ieum-diagram", move |request| handler.resource(request))?;
        Ok((session, policy))
});

fn shared() -> Result<&'static (tauri_runtime_cef::NativeNetworkSession, Arc<NetworkPolicy>), String> {
    SESSION.as_ref().map_err(Clone::clone)
}

pub(crate) fn session() -> Result<tauri_runtime_cef::NativeNetworkSession, String> {
    Ok(shared()?.0.clone())
}

pub(super) fn requests() -> Result<serde_json::Value, String> {
    Ok(shared()?.1.requests())
}

/// One native diagram session's request decisions and accumulated log.
pub(super) struct NetworkPolicy {
    log: Mutex<RequestLog>,
    dev_origins: Vec<String>,
}

impl NetworkPolicy {
    pub(super) fn new(dev_origins: Vec<String>) -> Self {
        Self { log: Mutex::new(new_request_log()), dev_origins }
    }

    /// `url` is the native interceptor's authenticated logical request URL.
    pub(super) fn allow(&self, url: &str) -> bool {
        let mut log = self.log.lock().unwrap_or_else(|error| error.into_inner());
        screen_request(&mut log, url, &self.dev_origins)
    }

    fn resource(&self, request: &tauri_runtime_cef::NetworkSessionRequest<'_>) -> bool {
        let original = if request.bound_protocol() == Some("genoffice-app-diagram") {
            let Ok(url) = tauri::Url::parse(request.url()) else { return false; };
            let Some(url) = crate::app_scheme::electron_url(&url) else { return false; };
            url
        } else {
            request.url().to_owned()
        };
        self.allow(&original)
    }

    pub(super) fn requests(&self) -> serde_json::Value {
        let log = self.log.lock().unwrap_or_else(|error| error.into_inner());
        serde_json::json!({ "allowed": log.allowed, "refused": log.refused, "blocked": log.blocked })
    }
}

#[cfg(test)]
mod tests {
    use super::NetworkPolicy;

    #[test]
    fn policy_records_real_decisions_and_redacts_query() {
        let policy = NetworkPolicy::new(Vec::new());
        assert!(policy.allow("data:image/png;base64,AA=="));
        assert!(!policy.allow("https://example.org/secret?document=private"));
        assert_eq!(policy.requests(), serde_json::json!({
            "allowed": 1, "refused": 1, "blocked": ["https://example.org/secret"]
        }));
    }
}
