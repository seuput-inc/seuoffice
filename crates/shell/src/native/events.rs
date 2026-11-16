//! What the native layer reports about one browser, always for the main frame.
//!
//! Events of one browser are delivered in order on the UI thread through [`EventSink`]
//! (see `defer.rs`). `DocumentRevoked { reason: Navigation }` precedes the
//! `NavigationStarted` that caused it.

use std::fmt;
use std::sync::Arc;

use tauri::Url;

use super::browser::BrowserIdentity;
use super::document::{DocumentContext, DocumentIdentity};
use super::lifecycle::BrowserCloseOutcome;
use super::navigation::NavigationIdentity;

/// Why a document stopped being live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeReason {
    /// A cross-document navigation was admitted, or an unadmitted commit replaced it.
    Navigation,
    /// The browser closed.
    Close,
    /// The web content process failed.
    ProcessFailed,
    /// Two different nonces were registered for one document; it fails closed.
    Conflict,
    /// A short effect run under the document gate panicked.
    Fault,
}

/// A navigation the OS webview reported as failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError {
    pub code: Option<i32>,
    pub description: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "{} (error {code})", self.description),
            None => f.write_str(&self.description),
        }
    }
}

/// The web content process of a browser is gone. No process identity is exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessFailure {
    /// WKWebView `webViewWebContentProcessDidTerminate`.
    #[cfg(target_os = "macos")]
    ContentProcessTerminated,
    /// WebView2 `ProcessFailed` with a render process kind.
    #[cfg(windows)]
    RenderProcessExited,
    /// WebView2 browser process exit.
    #[cfg(windows)]
    BrowserProcessExited,
    #[cfg(windows)]
    Unresponsive,
    /// WebView2 `ProcessFailed` of an unknown process kind.
    #[cfg(any(windows, test))]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewEvent {
    Focus,
    Blur,
}

#[derive(Clone, Debug)]
pub enum NativeEventKind {
    /// After the shell policy admitted a main-frame navigation.
    NavigationStarted {
        navigation: NavigationIdentity,
        url: Url,
        same_document: bool,
    },
    DocumentCommitted {
        navigation: NavigationIdentity,
        document: DocumentIdentity,
        url: Url,
    },
    /// The bridge registered its nonce for the committed document.
    DocumentRegistered {
        context: DocumentContext,
    },
    /// Registered, load finished and one macrotask passed (`render-ready`).
    DocumentReady {
        context: DocumentContext,
    },
    DocumentRevoked {
        document: DocumentIdentity,
        reason: RevokeReason,
    },
    /// Never inferred from "not loading".
    NavigationFailed {
        navigation: NavigationIdentity,
        url: Url,
        error: LoadError,
    },
    /// Always carries the outcome; `ok: false` follows a `NavigationFailed`.
    LoadFinished {
        navigation: NavigationIdentity,
        ok: bool,
    },
    AddressChanged {
        url: Url,
    },
    WebviewProcessFailed {
        kind: ProcessFailure,
    },
    View(ViewEvent),
    /// Browser receipt: the view is gone from Tauri's registry.
    Retired {
        outcome: BrowserCloseOutcome,
    },
}

#[derive(Clone)]
pub struct NativeEvent {
    pub browser: BrowserIdentity,
    pub kind: NativeEventKind,
}

impl fmt::Debug for NativeEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeEvent")
            .field("browser", &self.browser)
            .field("kind", &self.kind)
            .finish()
    }
}

/// Called on the UI thread; it must return promptly.
pub type EventSink = Arc<dyn Fn(&NativeEvent) + Send + Sync + 'static>;
