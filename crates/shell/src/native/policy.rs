//! Per-webview request policy.
//!
//! Replaces the CEF network session handler. The decision function is pure; how it is
//! enforced is per OS in `platform/*` (`PolicyOps::apply`):
//!
//! - Windows: a `WebResourceRequested` filter answers allow or block per request, with the
//!   exact decision.
//! - macOS has no public per-request interception for http(s). The adapter combines a
//!   `WKContentRuleList` built from the policy (all `http(s)`/`ws(s)` blocked except what the
//!   policy names), the shell's own scheme handlers adding a CSP to target documents, and
//!   `allowsContentJavaScript = false`. Requests the shell's handlers never see cannot be
//!   logged there.
//!
//! The top-level navigation allow check also runs inside the `on_navigation` hook on both
//! OSes, so a top-level navigation refused by the policy never starts.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// One request as the policy sees it.
pub struct PolicyRequest<'a> {
    pub url: &'a str,
    /// The shell scheme the request was served through (`genoffice-app-diagram`), if any.
    pub bound_scheme: Option<&'a str>,
    /// True for the top-level navigation check. The navigation hook cannot tell frames apart
    /// on macOS, so frame navigations are checked as top-level there too.
    pub top_level: bool,
}

/// Where the webview keeps its cookies, storage and cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    /// Tauri's default per-app store.
    Default,
    /// A named, persistent store: `data_store_identifier` on macOS 14+, a dedicated data
    /// directory on Windows.
    Isolated([u8; 16]),
}

impl Storage {
    /// A stable store identity for a human name ("ieum-diagram").
    pub fn named(name: &str) -> Self {
        let digest = Sha256::digest(format!("seuoffice-storage:{name}").as_bytes());
        let mut id = [0u8; 16];
        id.copy_from_slice(&digest[..16]);
        Self::Isolated(id)
    }
}

/// How the OS webview is told where to keep a store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    /// macOS 14+: `WebviewBuilder::data_store_identifier`.
    Identifier([u8; 16]),
    /// Windows: `WebviewBuilder::data_directory`.
    Directory(PathBuf),
}

fn hex(id: &[u8; 16]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The store setting for a webview. `scratch_root` is the per-run `SEUOFFICE_USER_DATA`
/// folder (a test driver, the measurement): with it every webview gets a store of its own
/// under that run, never the user's. `None` keeps Tauri's default store.
pub(crate) fn store_for(storage: Storage, scratch_root: Option<&Path>) -> Option<Store> {
    let id = match (storage, scratch_root) {
        (Storage::Isolated(id), None) => id,
        (Storage::Isolated(id), Some(root)) => {
            let mut digest = Sha256::new();
            digest.update(id);
            digest.update(root.as_os_str().as_encoded_bytes());
            let mut mixed = [0u8; 16];
            mixed.copy_from_slice(&digest.finalize()[..16]);
            mixed
        }
        (Storage::Default, Some(root)) => {
            let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
            let mut id = [0u8; 16];
            id.copy_from_slice(&digest[..16]);
            id
        }
        (Storage::Default, None) => return None,
    };
    if cfg!(target_os = "macos") {
        return Some(Store::Identifier(id));
    }
    let base = match scratch_root {
        Some(root) => root.join("webview2"),
        None => dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("SeuOffice")
            .join("webview-stores"),
    };
    Some(Store::Directory(base.join(hex(&id))))
}

/// The per-run `SEUOFFICE_USER_DATA` folder, when a test driver or the measurement set one.
pub(crate) fn scratch_root() -> Option<PathBuf> {
    std::env::var_os("SEUOFFICE_USER_DATA").filter(|value| !value.is_empty()).map(PathBuf::from)
}

type Allow = Arc<dyn Fn(&PolicyRequest<'_>) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct RequestPolicy {
    allow: Allow,
    javascript: bool,
    storage: Storage,
}

impl fmt::Debug for RequestPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestPolicy")
            .field("javascript", &self.javascript)
            .field("storage", &self.storage)
            .finish_non_exhaustive()
    }
}

impl RequestPolicy {
    pub fn new(allow: impl Fn(&PolicyRequest<'_>) -> bool + Send + Sync + 'static) -> Self {
        Self { allow: Arc::new(allow), javascript: true, storage: Storage::Default }
    }

    pub fn allow_all() -> Self {
        Self::new(|_| true)
    }

    pub fn deny_all() -> Self {
        Self::new(|_| false)
    }

    /// Print and target windows that must not run page scripts.
    pub fn with_javascript(mut self, enabled: bool) -> Self {
        self.javascript = enabled;
        self
    }

    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.storage = storage;
        self
    }

    pub fn allows(&self, request: &PolicyRequest<'_>) -> bool {
        (self.allow)(request)
    }

    pub fn javascript(&self) -> bool {
        self.javascript
    }

    pub fn storage(&self) -> Storage {
        self.storage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_decision_function_sees_the_request_and_defaults_allow_scripts() {
        let policy = RequestPolicy::new(|request| {
            request.top_level && request.url.starts_with("genoffice-app-shell:")
        });
        let page = PolicyRequest {
            url: "genoffice-app-shell://localhost/",
            bound_scheme: None,
            top_level: true,
        };
        let frame = PolicyRequest { url: page.url, bound_scheme: None, top_level: false };
        assert!(policy.allows(&page));
        assert!(!policy.allows(&frame));
        assert!(policy.javascript());
        assert!(!policy.clone().with_javascript(false).javascript());
        assert_eq!(policy.storage(), Storage::Default);
    }

    #[test]
    fn named_storage_is_stable_and_distinct_per_name() {
        assert_eq!(Storage::named("ieum-diagram"), Storage::named("ieum-diagram"));
        assert_ne!(Storage::named("ieum-diagram"), Storage::named("other"));
        assert!(matches!(Storage::named("x"), Storage::Isolated(_)));
    }

    #[test]
    fn allow_all_and_deny_all_are_total() {
        let request = PolicyRequest { url: "https://example.org/", bound_scheme: None, top_level: false };
        assert!(RequestPolicy::allow_all().allows(&request));
        assert!(!RequestPolicy::deny_all().allows(&request));
    }
}
