use std::sync::{Arc, LazyLock, Mutex};

use main_core::diagram::drawio_protocol::{DRAWIO_HOST, DRAWIO_SCHEME};
use main_core::diagram::network_guard::{RequestLog, new_request_log, screen_request};

use crate::native::{Blocks, PolicyRequest, RequestPolicy, Storage};

/// The diagram tab's persistent store: draw.io keeps its own storage in it.
const STORE: &str = "ieum-diagram";

/// The scheme the shell serves the diagram renderer through.
const DIAGRAM_APP_SCHEME: &str = "genoffice-app-diagram";

static POLICY: LazyLock<Arc<NetworkPolicy>> = LazyLock::new(|| {
    let diagram_url = std::env::var("DIAGRAM_RENDERER_URL").ok();
    let electron_url = std::env::var("ELECTRON_RENDERER_URL").ok();
    let origins = main_core::diagram::network_guard::dev_server_origins(
        &[None, diagram_url.as_deref(), electron_url.as_deref()],
        !cfg!(debug_assertions),
    );
    Arc::new(NetworkPolicy::new(origins))
});

/// The one policy of every diagram webview: they share one decision function and one log.
pub(crate) fn request_policy() -> RequestPolicy {
    let policy = POLICY.clone();
    RequestPolicy::new(move |request| policy.resource(request))
        .with_storage(Storage::named(STORE))
        .with_blocks(POLICY.blocks())
}

pub(super) fn requests() -> Result<serde_json::Value, String> {
    Ok(POLICY.requests())
}

/// One native diagram session's request decisions and accumulated log.
pub(super) struct NetworkPolicy {
    log: Mutex<RequestLog>,
    dev_origins: Vec<String>,
}

impl NetworkPolicy {
    pub(super) fn new(dev_origins: Vec<String>) -> Self {
        Self {
            log: Mutex::new(new_request_log()),
            dev_origins,
        }
    }

    /// What the policy refuses as a class, for platforms that block by class: every web
    /// request (loopback stays open only when a dev server origin is allowed) and files.
    pub(super) fn blocks(&self) -> Blocks {
        Blocks {
            web: true,
            loopback_open: !self.dev_origins.is_empty(),
            files: true,
        }
    }

    /// `url` is the logical request URL, as the page's scheme names it.
    pub(super) fn allow(&self, url: &str) -> bool {
        let mut log = self.log.lock().unwrap_or_else(|error| error.into_inner());
        screen_request(&mut log, url, &self.dev_origins)
    }

    /// The request as the page's scheme names it. The platform shape of a shell scheme
    /// (`http://<scheme>.localhost/` on Windows) is translated back first; a request the
    /// shell cannot read back is refused.
    fn logical_url(request: &PolicyRequest<'_>) -> Option<String> {
        match request.bound_scheme {
            Some(DIAGRAM_APP_SCHEME) => {
                let url = tauri::Url::parse(request.url).ok()?;
                crate::app_scheme::electron_url(&url)
            }
            Some(DRAWIO_SCHEME) => {
                let url = tauri::Url::parse(request.url).ok()?;
                if url.scheme() == DRAWIO_SCHEME {
                    return Some(request.url.to_owned());
                }
                // Only the scheme decides for the shell's own drawio files.
                Some(format!("{DRAWIO_SCHEME}://{DRAWIO_HOST}/"))
            }
            _ => Some(request.url.to_owned()),
        }
    }

    /// `about:blank` is where a protected navigation starts: it fetches nothing and is neither
    /// allowed nor refused traffic, so it is not logged.
    fn resource(&self, request: &PolicyRequest<'_>) -> bool {
        if request.url == "about:blank" {
            return true;
        }
        match Self::logical_url(request) {
            Some(original) => self.allow(&original),
            None => false,
        }
    }

    pub(super) fn requests(&self) -> serde_json::Value {
        let log = self.log.lock().unwrap_or_else(|error| error.into_inner());
        serde_json::json!({ "allowed": log.allowed, "refused": log.refused, "blocked": log.blocked })
    }
}

#[cfg(test)]
mod tests {
    use super::NetworkPolicy;
    use crate::native::PolicyRequest;

    fn request<'a>(url: &'a str, bound_scheme: Option<&'a str>) -> PolicyRequest<'a> {
        PolicyRequest {
            url,
            bound_scheme,
            top_level: false,
            main_frame: false,
        }
    }

    #[test]
    fn policy_records_real_decisions_and_redacts_query() {
        let policy = NetworkPolicy::new(Vec::new());
        assert!(policy.allow("data:image/png;base64,AA=="));
        assert!(!policy.allow("https://example.org/secret?document=private"));
        assert_eq!(
            policy.requests(),
            serde_json::json!({
                "allowed": 1, "refused": 1, "blocked": ["https://example.org/secret"]
            })
        );
    }

    #[test]
    fn the_shell_schemes_are_allowed_in_both_platform_shapes() {
        let policy = NetworkPolicy::new(Vec::new());
        assert!(policy.resource(&request(
            "drawio-app://editor/index.html",
            Some("drawio-app")
        )));
        assert!(policy.resource(&request(
            "http://drawio-app.localhost/editor/js/app.min.js?x=1",
            Some("drawio-app")
        )));
        assert!(!policy.resource(&request("http://example.org/", None)));
        assert!(!policy.resource(&request("not a url", Some("drawio-app"))));
        assert!(!policy.resource(&request("not a url", Some("genoffice-app-diagram"))));
        assert_eq!(
            policy.requests(),
            serde_json::json!({
                "allowed": 2, "refused": 1, "blocked": ["http://example.org/"]
            })
        );
    }

    #[test]
    fn about_blank_is_neither_allowed_nor_logged() {
        let policy = NetworkPolicy::new(Vec::new());
        assert!(policy.resource(&request("about:blank", None)));
        assert_eq!(
            policy.requests(),
            serde_json::json!({
                "allowed": 0, "refused": 0, "blocked": []
            })
        );
    }
}
