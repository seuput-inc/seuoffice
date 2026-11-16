//! Native shell effects for the PDF tools window.

use std::sync::Arc;

use main_core::shell::app_settings::read_app_settings;
use shell_contract::{ViewId, WindowId, runtime::Views};
use shell_ipc_d2::pdf::tools::{PdfTab, PdfToolsShell, PdfToolsShellHost, TabKind};
use shell_ipc_w::editor_files::UiLanguage;
use tauri::Manager;

use super::{tabs, windows};
use crate::host::{HostContext, w_files::ShellLanguage};
use crate::paths::ShellPaths;

pub(super) fn host(cx: &HostContext) -> PdfToolsShellHost {
    Arc::new(Shell { cx: cx.clone() })
}

struct Shell {
    cx: HostContext,
}

impl PdfToolsShell for Shell {
    fn random_uuid(&self) -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("operating system random source failed");
        bytes[6] = (bytes[6] & 15) | 0x40;
        bytes[8] = (bytes[8] & 63) | 0x80;
        format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15])
    }
    fn lang(&self) -> String { ShellLanguage(self.cx.ui.clone()).lang().as_str().to_owned() }
    fn theme(&self) -> String {
        let settings = read_app_settings(&ShellPaths::new().settings_file().to_string_lossy());
        match settings.get("theme") {
            Some(jscompat::JsonValue::String(value)) if value == "light" || value == "dark" => value.clone(),
            _ => "system".to_owned(),
        }
    }
    fn text(&self, key: &str, params: &[(&str, String)]) -> String {
        let lang = ShellLanguage(self.cx.ui.clone()).lang();
        let mut text = shell_ipc_m::menu_label(shell_ipc_m::MenuDict::Shell, lang, key);
        for (name, value) in params { text = text.replace(&format!("{{{name}}}"), value); }
        text
    }
    fn open_tools_window(&self, parent: Option<WindowId>, title: &str) -> Option<ViewId> { windows::open_tools_window(&self.cx, parent, title) }
    fn show_window(&self, view: ViewId) { windows::show_window(&self.cx, view); }
    fn open_password_window(&self, parent: Option<WindowId>, title: &str) -> Option<ViewId> { crate::host::document_password::open_password_window(&self.cx, parent, title) }
    fn open_password_window_owned(
        &self, parent: Option<WindowId>, title: String,
        source: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(ViewId, shell_contract::ipc::NativeRequestOwner), String>) + Send>,
    ) -> shell_ipc_d2::pdf::tools::OwnedPasswordWindowActions {
        crate::host::document_password::open_password_window_owned(&self.cx, parent, title, source, destination, cancel, pending, done)
    }
    fn open_password_window_prepared(
        &self, parent: Option<WindowId>, title: String, target: shell_contract::ipc::RecoveryOpenTarget, snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(ViewId, shell_contract::ipc::NativeRequestOwner), String>) + Send>,
    ) -> shell_ipc_d2::pdf::tools::OwnedPasswordWindowActions {
        crate::host::document_password::open_password_window_prepared(&self.cx, parent, title, target, snapshot, cancel, pending, done)
    }
    fn open_result(&self, path: &str) -> bool { tabs::open_result(path, true) }
    fn open_for_editing(&self, path: &str) { tabs::open_for_editing(path); }
    fn file_has_unsaved_changes(&self, path: &str, then: Box<dyn FnOnce(bool)>) { tabs::file_has_unsaved_changes(path, then); }
    fn open_document_path(&self, path: &str, then: Box<dyn FnOnce(bool) + Send>) { tabs::open_document_path(path, then); }
    fn active_pdf_tab(&self) -> Option<PdfTab> { tabs::active_pdf_tab() }
    fn flush_pdf_save(&self, view: ViewId, then: Box<dyn FnOnce(bool)>) { tabs::flush_pdf_save(view, then); }
    fn find_tab(&self, kind: TabKind, path: &str) -> Option<String> { tabs::find_tab(kind, path) }
    fn close_tab(&self, tab: &str, then: Box<dyn FnOnce(Result<(), String>)>) { tabs::close_tab(tab, then); }
    fn activate_tab(&self, tab: &str) { tabs::activate_tab(tab); }
    fn activate_view(&self, view: ViewId) { tabs::activate_view(view); }
    fn set_progress_bar(&self, window: WindowId, progress: f64) {
        use tauri::window::{ProgressBarState, ProgressBarStatus};
        let Some(label) = self.cx.views.label_of_window(window) else { return };
        let app = self.cx.app.clone();
        crate::host::on_ui(&self.cx.ui, move || {
            if let Some(window) = app.get_window(&label) {
                let state = if progress < 0.0 { ProgressBarState { status: Some(ProgressBarStatus::None), progress: None } }
                    else if progress > 1.0 { ProgressBarState { status: Some(ProgressBarStatus::Indeterminate), progress: None } }
                    else { ProgressBarState { status: Some(ProgressBarStatus::Normal), progress: Some((progress * 100.0).round() as u64) } };
                let _ = window.set_progress_bar(state);
            }
        });
    }
    fn has_tab_manager(&self) -> bool { tabs::has_tab_manager() }
    fn open_hwp_copy(&self, copy: &str, title: &str, then: Box<dyn FnOnce(Result<Option<ViewId>, String>)>) { tabs::open_hwp_copy(copy, title, then); }
}
