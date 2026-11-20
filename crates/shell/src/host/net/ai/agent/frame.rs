//! The agent window's frame (`openAgentWindow` of `agent-window.ts`): the one
//! window of the agent, made through the shell's window host so the page gets
//! the `ShellAgent` bridge and every native guard the other pages have.
//!
//! What `new BrowserWindow(options)` of the TypeScript asked for is what
//! `agent_window_options` answers (size, minimum size, centred, not
//! full-screenable, hidden until `ready-to-show`, the localized title). The
//! page is the shell host's `agent.html`, served by the app scheme; the
//! Electron build's preload file and dev-server address have no counterpart
//! (the bridge is injected by the window host, the renderer bundle is read
//! through the scheme), so the frame reports what is there instead.
//!
//! The window-open handler (a safe URL goes to the OS, the window is denied) is
//! the window host's popup table for the `ShellAgent` surface
//! (`window_open::policy_of`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use shell_contract::{Surface, ViewId, WindowId};
use shell_ipc_a::agent::AgentWindowFrame;

use crate::host::window::{Bridge, ViewSpec, WindowOptions, WindowSpec, windows};
use crate::native::{NativeEvent, NativeEventKind};

/// The agent page, as every handler names a page of the shell host
const AGENT_PAGE: &str = "genoffice-app://shell/agent.html";

/// The open window: its window and the page in it
type Open = Option<(WindowId, ViewId)>;

#[derive(Default)]
pub(super) struct Frame {
    open: Arc<Mutex<Open>>,
}

fn lock(open: &Mutex<Open>) -> std::sync::MutexGuard<'_, Open> {
    open.lock().unwrap_or_else(PoisonError::into_inner)
}

fn number(options: &Value, key: &str) -> Option<f64> {
    options.get(key).and_then(Value::as_f64)
}

/// `agentText(lang, 'windowTitle')`: the renderer's string table
fn title_in(lang: &str) -> &'static str {
    if lang == "ko" {
        "에이전트"
    } else {
        "Agent"
    }
}

impl Frame {
    /// The window of the open page
    fn window(&self) -> Option<tauri::Window> {
        let (_, view) = (*lock(&self.open))?;
        windows()?.webview(view).map(|webview| webview.window())
    }
}

impl AgentWindowFrame for Frame {
    fn view(&self) -> Option<ViewId> {
        lock(&self.open).map(|(_, view)| view)
    }

    fn is_minimized(&self) -> bool {
        self.window()
            .is_some_and(|window| window.is_minimized().unwrap_or(false))
    }

    fn reveal(&self, restore: bool) {
        let Some(window) = self.window() else {
            return;
        };
        if restore && let Err(error) = window.unminimize() {
            eprintln!("shell: restoring the agent window: {error}");
        }
        if let Err(error) = window.show() {
            eprintln!("shell: showing the agent window: {error}");
        }
        if let Err(error) = window.set_focus() {
            eprintln!("shell: focusing the agent window: {error}");
        }
    }

    fn window_title(&self, lang: &str) -> String {
        title_in(lang).to_owned()
    }

    /// The bridge is injected into the page by the window host: there is no preload file
    fn preload_path(&self) -> String {
        String::new()
    }

    /// The renderer is read through the app scheme: there is no dev server to load from
    fn renderer_url(&self) -> Option<String> {
        None
    }

    /// The page is the app-scheme file the create step checks against
    fn built_file(&self) -> String {
        AGENT_PAGE.to_owned()
    }

    fn create(&self, options: &Value, load: &Value) -> Result<ViewId, String> {
        let page = load
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or(AGENT_PAGE);
        if page != AGENT_PAGE {
            return Err(format!("'{page}' is not the agent page"));
        }
        let windows = windows().ok_or("The window host is not installed.")?;
        let (width, height) = (
            number(options, "width").unwrap_or(560.0),
            number(options, "height").unwrap_or(760.0),
        );
        let min_size = match (number(options, "minWidth"), number(options, "minHeight")) {
            (Some(width), Some(height)) => Some((width, height)),
            _ => None,
        };
        let spec = WindowSpec {
            title: options
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            size: (width, height),
            min_size,
            // shown on `ready-to-show`
            visible: false,
            decorations: true,
            transparent: false,
            resizable: true,
            parent: None,
            focus: true,
            main: false,
            view: ViewSpec {
                url: AGENT_PAGE.to_owned(),
                bridge: Bridge::Surface(Surface::ShellAgent),
            },
        };
        let shown = AtomicBool::new(false);
        let host = windows.clone();
        let on_frame = Arc::new(move |view: ViewId, event: &NativeEvent| {
            let ready = match &event.kind {
                NativeEventKind::DocumentReady { .. } => true,
                NativeEventKind::LoadFinished { ok: false, .. } => false,
                _ => return,
            };
            if shown.swap(true, Ordering::AcqRel) {
                return;
            }
            let Some(window) = host.webview(view).map(|webview| webview.window()) else {
                return;
            };
            if ready {
                if let Err(error) = window.show().and_then(|()| window.set_focus()) {
                    eprintln!("shell: showing the agent window: {error}");
                }
            } else {
                // a window that never shows would stay behind unseen
                eprintln!("shell: the agent page did not load; closing its window");
                if let Err(error) = window.destroy() {
                    eprintln!("shell: closing the agent window: {error}");
                }
            }
        });
        let open = self.open.clone();
        let extras = WindowOptions {
            center: options
                .get("center")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            fullscreenable: options
                .get("fullscreenable")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            on_frame: Some(on_frame),
            on_closed: Some(Box::new(move |window| {
                let mut open = lock(&open);
                if open.is_some_and(|(current, _)| current == window) {
                    *open = None;
                }
            })),
            ..WindowOptions::default()
        };
        let (window, view) = windows.open_window_with_options(spec, extras)?;
        *lock(&self.open) = Some((window, view));
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_follows_the_language() {
        let frame = Frame::default();
        assert_eq!(frame.window_title("ko"), "에이전트");
        assert_eq!(frame.window_title("en"), "Agent");
        assert_eq!(frame.window_title("fr"), "Agent");
    }

    #[test]
    fn a_frame_with_no_window_reports_none_and_refuses_another_page() {
        let frame = Frame::default();
        assert_eq!(frame.view(), None);
        assert!(!frame.is_minimized());
        frame.reveal(true);
        assert_eq!(frame.renderer_url(), None);
        assert_eq!(frame.built_file(), AGENT_PAGE);
        let refused = frame.create(
            &serde_json::json!({}),
            &serde_json::json!({ "file": "/elsewhere/agent.html" }),
        );
        assert!(refused.is_err());
    }
}
