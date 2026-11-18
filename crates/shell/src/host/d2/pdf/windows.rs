//! Native windows for the PDF tools surface.

use std::sync::Arc;
use shell_contract::{Surface, ViewId, WindowId, runtime::Views};
use tauri::Manager;
use tauri_runtime_cef::FrameEventKind;
use crate::host::{HostContext, window::{Bridge, ViewSpec, WindowOptions, WindowSpec}};

pub(super) fn open_tools_window(cx: &HostContext, parent: Option<WindowId>, title: &str) -> Option<ViewId> {
    let app = cx.app.clone();
    let views = cx.views.clone();
    let shown = std::sync::atomic::AtomicBool::new(false);
    let on_frame = Arc::new(move |view: ViewId, event: &tauri_runtime_cef::FrameEvent| {
        if event.is_main && matches!(event.kind, FrameEventKind::LoadingStateChanged { is_loading: false }) && !shown.swap(true, std::sync::atomic::Ordering::AcqRel) {
            if let Some(window) = views.sender_of(view).and_then(|sender| views.label_of_window(sender.window)).and_then(|label| app.get_window(&label)) {
                let _ = window.show();
            }
        }
    });
    let spec = WindowSpec {
        title: title.to_owned(), size: (760.0, 720.0), min_size: Some((520.0, 520.0)),
        visible: false, decorations: true, transparent: false, resizable: true,
        parent, focus: true, main: false,
        view: ViewSpec { url: "genoffice-app://shell/pdf-tools.html".into(), bridge: Bridge::Surface(Surface::ShellPdfTools) },
    };
    let options = WindowOptions { center: true, minimizable: false, maximizable: false, fullscreenable: false, on_frame: Some(on_frame), ..Default::default() };
    match cx.windows.open_window_with_options(spec, options) {
        Ok((_, view)) => Some(view),
        Err(error) => { eprintln!("[pdf] could not open the tools window: {error}"); None }
    }
}

pub(super) fn show_window(cx: &HostContext, view: ViewId) {
    let Some(label) = cx.views.sender_of(view).and_then(|sender| cx.views.label_of_window(sender.window)) else { return };
    let app = cx.app.clone();
    crate::host::on_ui(&cx.ui, move || {
        if let Some(window) = app.get_window(&label) {
            if window.is_minimized().unwrap_or(false) { let _ = window.unminimize(); }
            let _ = window.show();
            let _ = window.set_focus();
        }
    });
}
