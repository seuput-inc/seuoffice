//! Logged stubs of batch W's host traits (`crates/shell-ipc/w`) whose real
//! implementation is a later unit: the tab strip and window system
//! (`WindowsHost`), the editors' tab and file facts, the home screen's file
//! index, tree and watcher, the app facts, the preview and picker windows.
//!
//! Each call logs its trait and method once, then answers what an app with
//! nothing open would: empty, `false`, an error that says "not built yet". A
//! call that has no such answer (it must hand back a window or a view) panics
//! with the same words; the dispatch turns that into an error reply.
//!
//! Written from the compiler's list of missing items: when a trait of batch
//! W changes, rustc names what the impl lacks, and the lines below follow it.
//! `TabHooks` lives on the core thread and is provided, not hosted
//! ([`super::w::register_hooks`]).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use main_core::packages::ai_provider::settings_secrets::AiSecretCodec;
use main_core::packages::electron_utils::default_save_dir::Thrown;
use main_core::packages::electron_utils::unsaved_files::{OpenFileState, OpenFilesSourceFailed};
use main_core::packages::i18n::Lang;
use main_core::packages::project_store::LegacyDocumentChat;
use main_core::shell::detached_windows::WindowOptions;
use main_core::shell::doc_picker::asker::DocAsker;
use main_core::shell::doc_picker::doc_list::{OpenDocumentEntry, ProjectFile, RecentDocumentEntry};
use main_core::shell::projects::memory::LocalDateParts;
use main_core::shell::projects::project_tools::{
    IndexSearchOptions, IndexSearchResult, KbReadPage,
};
use main_core::shell::tab_manager::{
    DiagramIdentity, LoadedTabView, Opener, SplitViewState, TabError, TabKind,
    UnloadedTabOpenContext,
};
use serde_json::Value;
use shell_contract::error::IpcError;
use shell_contract::ids::{ViewId, WindowId};
use shell_contract::value::IpcValue;
use shell_ipc_w::doc_picker::*;
use shell_ipc_w::editor_files::{DocsTab, DocsTabs, FileParse, OpenFiles, SheetsTabs, UiLanguage};
use shell_ipc_w::home_files::*;
use shell_ipc_w::home_settings::*;
use shell_ipc_w::projects::host::{
    LegacyProjectStore, PageEvents, ProjectFiles, ProjectTimer, ProjectsHost,
};
use shell_ipc_w::quick_look::*;
use shell_ipc_w::tabs::*;

const NOT_BUILT: &str = "not built yet";

/// What is not built is said once per call site
fn unbuilt(what: &'static str) {
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap();
    if !seen.contains(&what) {
        seen.push(what);
        eprintln!("shell: not built yet: {what}");
    }
}

fn not_running() -> McpStatus {
    McpStatus {
        running: false,
        settings: McpSettings {
            enabled: false,
            port: DEFAULT_MCP_PORT,
            background: false,
            logging: false,
        },
        url: None,
        capabilities: Vec::new(),
    }
}

pub(super) struct Unbuilt;

impl QuickLookRuntime for Unbuilt {
    fn attach(&self, _: QuickLookControl) {
        unbuilt("QuickLookRuntime::attach");
    }
    fn windows_explorer(&self) -> bool {
        unbuilt("QuickLookRuntime::windows_explorer");
        false
    }
    fn sync(&self, _: &QuickLookSettings) {
        unbuilt("QuickLookRuntime::sync");
    }
    fn refresh_tray(&self) {
        unbuilt("QuickLookRuntime::refresh_tray");
    }
    fn stop(&self) {
        unbuilt("QuickLookRuntime::stop");
    }
}

impl QuickLookWindow for Unbuilt {
    fn attach(&self, _: QuickLookControl) {
        unbuilt("QuickLookWindow::attach");
    }
    fn lang(&self) -> Lang {
        unbuilt("QuickLookWindow::lang");
        Lang::Ko
    }
    fn theme(&self) -> String {
        unbuilt("QuickLookWindow::theme");
        String::new()
    }
    fn create(&self, _: &QuickLookWindowOptions) -> ViewId {
        unbuilt("QuickLookWindow::create");
        panic!("QuickLookWindow::create is not built yet")
    }
    fn is_visible(&self) -> bool {
        unbuilt("QuickLookWindow::is_visible");
        false
    }
    fn set_title(&self, _: &str) {
        unbuilt("QuickLookWindow::set_title");
    }
    fn show_inactive(&self) {
        unbuilt("QuickLookWindow::show_inactive");
    }
    fn hide(&self) {
        unbuilt("QuickLookWindow::hide");
    }
    fn destroy(&self) {
        unbuilt("QuickLookWindow::destroy");
    }
    fn open_in_editor(&self, _: &str) {
        unbuilt("QuickLookWindow::open_in_editor");
    }
}

impl main_core::shell::quick_look::QuickLookEngines for Unbuilt {
    fn parse_file_to_text(
        &self,
        _: &str,
    ) -> Result<main_core::shell::quick_look::ParsedFile, String> {
        unbuilt("QuickLookEngines::parse_file_to_text");
        Err(NOT_BUILT.into())
    }
    fn open_zip(&self, _: Vec<u8>) -> Option<Box<dyn main_core::shell::quick_look::ZipArchive>> {
        unbuilt("QuickLookEngines::open_zip");
        None
    }
}

impl DocPickerWindow for Unbuilt {
    fn attach(&self, _: DocPickerControl) {
        unbuilt("DocPickerWindow::attach");
    }
    fn lang(&self) -> Lang {
        unbuilt("DocPickerWindow::lang");
        Lang::Ko
    }
    fn theme(&self) -> String {
        unbuilt("DocPickerWindow::theme");
        String::new()
    }
    fn create(&self, _: &DocPickerWindowOptions) -> ViewId {
        unbuilt("DocPickerWindow::create");
        panic!("DocPickerWindow::create is not built yet")
    }
    fn renderer_ready(&self, _: ViewId) {
        unbuilt("DocPickerWindow::renderer_ready");
    }
    fn show(&self) {
        unbuilt("DocPickerWindow::show");
    }
    fn focus(&self) {
        unbuilt("DocPickerWindow::focus");
    }
    fn close(&self) {
        unbuilt("DocPickerWindow::close");
    }
}

impl DocPickerSources for Unbuilt {
    fn asker_for(&self, _: ViewId) -> DocAsker {
        unbuilt("DocPickerSources::asker_for");
        DocAsker::default()
    }
}

impl main_core::shell::doc_picker::doc_list::DocListDeps for Unbuilt {
    fn open_documents(&self) -> Result<Vec<OpenDocumentEntry>, String> {
        Err("document hosts have not finished starting".to_owned())
    }
    fn recent(&self) -> Vec<RecentDocumentEntry> {
        unbuilt("DocListDeps::recent");
        Vec::new()
    }
    fn project_files(&self, _: &DocAsker) -> Vec<ProjectFile> {
        unbuilt("DocListDeps::project_files");
        Vec::new()
    }
    fn stat(&self, _: &str) -> Option<f64> {
        unbuilt("DocListDeps::stat");
        None
    }
}

impl DocsTabs for Unbuilt {
    fn open_tab_owned(
        &self, _: Option<String>, _: bool, _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        done(Err("Docs tab creation is unavailable in Unbuilt".into()));
    }
    fn list_tabs(&self) -> Vec<DocsTab> {
        unbuilt("DocsTabs::list_tabs");
        Vec::new()
    }
    fn focus_tab(&self, _: &str) {
        unbuilt("DocsTabs::focus_tab");
    }
}

impl OpenFiles for Unbuilt {
    fn open_files(&self) -> Result<Vec<OpenFileState>, OpenFilesSourceFailed> {
        unbuilt("OpenFiles::open_files");
        panic!("OpenFiles::open_files is not built yet")
    }
}

impl FileParse for Unbuilt {
    fn parse_file_to_text(
        &self,
        _: &std::path::Path,
    ) -> Result<shell_ipc_w::editor_files::ParsedFile, String> {
        unbuilt("FileParse::parse_file_to_text");
        Err(NOT_BUILT.into())
    }
    fn diagram_picture_ext(&self, _: &std::path::Path) -> Option<&'static str> {
        unbuilt("FileParse::diagram_picture_ext");
        None
    }
}

impl UiLanguage for Unbuilt {
    fn lang(&self) -> Lang {
        unbuilt("UiLanguage::lang");
        Lang::Ko
    }
}

impl SheetsTabs for Unbuilt {
    fn is_tab(&self, _: ViewId) -> bool {
        unbuilt("SheetsTabs::is_tab");
        false
    }
    fn external_link_source(&self, _: ViewId, _: &str, _: u64) -> Option<String> {
        unbuilt("SheetsTabs::external_link_source");
        None
    }
}

impl HomeFiles for Unbuilt {
    fn now_ms(&self) -> f64 {
        unbuilt("HomeFiles::now_ms");
        0.0
    }
    fn system_locale(&self) -> String {
        unbuilt("HomeFiles::system_locale");
        String::new()
    }
    fn open_document_path_owned(
        &self,_:String,_:shell_contract::ipc::NativeRequestOwner,_:shell_contract::runtime::CancelToken,
        done:Box<dyn FnOnce(Result<Option<shell_contract::ViewId>,String>)+Send>,
    ) { done(Err("HomeFiles document opener is not built".into())); }
    fn open_dropped_files(&self, _: Vec<String>) {
        unbuilt("HomeFiles::open_dropped_files");
    }
    fn open_file_paths(&self) -> Vec<String> {
        unbuilt("HomeFiles::open_file_paths");
        Vec::new()
    }
    fn project_file_paths(&self) -> Vec<String> {
        unbuilt("HomeFiles::project_file_paths");
        Vec::new()
    }
    fn slides_recent_files(&self) -> Vec<String> {
        unbuilt("HomeFiles::slides_recent_files");
        Vec::new()
    }
    fn document_link_paths(&self) -> Vec<String> {
        crate::host::menu::document_link_paths()
    }
    fn project_file_renamed(&self, _: &str, _: &str) {
        unbuilt("HomeFiles::project_file_renamed");
    }
    fn file_moved_before_template(&self, old_path: &str, new_path: &str) {
        crate::host::menu::document_link_file_moved(old_path, new_path);
        unbuilt("HomeFiles::file_moved_before_template");
    }
    fn file_moved(&self, _: &str, _: &str) {
        unbuilt("HomeFiles::file_moved");
    }
    fn recents_changed(&self) {
        unbuilt("HomeFiles::recents_changed");
    }
    fn prepare_recents_changed_owned(
        &self, _: &shell_contract::ipc::NativeRequestOwner, _: &shell_contract::ipc::NativeRequestOwner,
        _: &shell_contract::runtime::CancelToken, _: &shell_contract::runtime::CancelToken,
        _: &shell_contract::hosts::OwnedEffectsHost,
    )->Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>,String> {
        Err("HomeFiles owned menu refresh is not built".into())
    }
    fn prepare_recents_changed_many_owned(
        &self,_:&shell_contract::ipc::NativeRequestOwner,_:&shell_contract::ipc::NativeRequestOwner,
        _:&[shell_contract::ipc::NativeRequestOwner],
        _:&shell_contract::runtime::CancelToken,_:&shell_contract::runtime::CancelToken,
        _:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>,String> {
        Err("HomeFiles owned many-participant menu refresh is not built".into())
    }
    fn prepare_recents_changed_many_owned_extra(
        &self,_:&shell_contract::ipc::NativeRequestOwner,_:&shell_contract::ipc::NativeRequestOwner,
        _:&[shell_contract::ipc::NativeRequestOwner],_:&shell_contract::runtime::CancelToken,
        _:&shell_contract::runtime::CancelToken,_:&[shell_contract::runtime::CancelToken],
        _:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>,String> {
        Err("HomeFiles owned extra menu refresh is not built".into())
    }
    fn prepare_recents_changed_recovery(
        &self, _: &shell_contract::ipc::RecoverySeedReceipt, _: &shell_contract::ipc::NativeRequestOwner,
        _: &shell_contract::runtime::CancelToken, _: &shell_contract::runtime::CancelToken,
        _: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        Err("HomeFiles recovery menu refresh is not built".into())
    }
    fn prepare_recents_changed_native(
        &self, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand,
        _: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        Err("HomeFiles native menu refresh is not built".into())
    }
    fn prepare_recents_changed_existing_document(
        &self, _: &shell_contract::ipc::NativeExistingDocumentBinding, _: &[shell_contract::ipc::NativeRequestOwner],
        _: &shell_contract::runtime::CancelToken, _: &shell_contract::runtime::CancelToken, _: &[shell_contract::runtime::CancelToken],
        _: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        Err("HomeFiles existing-document menu refresh is not built".into())
    }
    fn prepare_recents_changed_saved_document(
        &self, _: &shell_contract::ipc::NativeSavedDocument,
        _: &[shell_contract::ipc::NativeRequestOwner],
        _: &shell_contract::runtime::CancelToken, _: &shell_contract::runtime::CancelToken,
        _: &[shell_contract::runtime::CancelToken], _: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_w::home_files::OwnedHomeRefresh>, String> {
        Err("HomeFiles saved-document menu refresh is unavailable in Unbuilt".into())
    }
    fn open_recycle_bin(&self) {
        unbuilt("HomeFiles::open_recycle_bin");
    }
}

impl FileSearch for Unbuilt {
    fn available(&self) -> Result<bool, String> {
        unbuilt("FileSearch::available");
        Err(NOT_BUILT.into())
    }
    fn refresh_if_stale(&self, _: f64) {
        unbuilt("FileSearch::refresh_if_stale");
    }
    fn refresh(&self) {
        unbuilt("FileSearch::refresh");
    }
    fn search(&self, _: &str, _: Option<&[String]>, _: f64, _: f64) -> Result<SearchResult, String> {
        unbuilt("FileSearch::search");
        Err(NOT_BUILT.into())
    }
    fn progress(&self) -> Result<IndexProgress, String> {
        unbuilt("FileSearch::progress");
        Err(NOT_BUILT.into())
    }
    fn rerank(&self, _: &str, _: &[String]) -> Result<Option<IpcValue>, String> {
        unbuilt("FileSearch::rerank");
        Err(NOT_BUILT.into())
    }
}

impl FolderWatch for Unbuilt {
    fn watch(&self, _: &[String], _: Arc<dyn Fn(Vec<String>) + Send + Sync>) {
        unbuilt("FolderWatch::watch");
    }
}

impl HomeApp for Unbuilt {
    fn version(&self) -> String {
        unbuilt("HomeApp::version");
        String::new()
    }
    fn is_packaged(&self) -> bool {
        unbuilt("HomeApp::is_packaged");
        false
    }
    fn locale(&self) -> String {
        unbuilt("HomeApp::locale");
        String::new()
    }
    fn os_release(&self) -> Result<String, String> {
        unbuilt("HomeApp::os_release");
        Err(NOT_BUILT.into())
    }
    fn now_ms(&self) -> f64 {
        unbuilt("HomeApp::now_ms");
        0.0
    }
    fn third_party_notices_path(&self) -> PathBuf {
        unbuilt("HomeApp::third_party_notices_path");
        PathBuf::new()
    }
    fn open_help_owned(&self, _:Option<String>, _:shell_contract::ipc::NativeRequestOwner,
        _:shell_contract::runtime::CancelToken, _:shell_contract::runtime::CancelToken,
        done:Box<dyn FnOnce(Result<(),String>)+Send>) {
        unbuilt("HomeApp::open_help_owned");
        done(Err(NOT_BUILT.into()));
    }
    fn auto_update_check_changed(&self, _: bool) {
        unbuilt("HomeApp::auto_update_check_changed");
    }
    fn delimited_import_limit_changed(&self, _: f64) {
        unbuilt("HomeApp::delimited_import_limit_changed");
    }
    fn home_kinds_changed(&self) {
        super::menu::home_kinds_changed();
    }
    fn begin_save_dir_change(&self) -> Result<(), String> {
        Err("HomeApp::begin_save_dir_change is unavailable".into())
    }
    fn end_save_dir_change(&self) -> Result<(), String> {
        Err("HomeApp::end_save_dir_change is unavailable".into())
    }
    fn folder_watchers_changed(&self, done: Box<dyn FnOnce(Result<(), IpcError>) + Send>) {
        done(Err(IpcError { name: Some("Error".into()), message: "HomeApp::folder_watchers_changed is unavailable".into() }));
    }
    /// batch A's codec over safeStorage (`host/secrets.rs` sets it)
    fn secrets(&self) -> Option<&dyn AiSecretCodec> {
        super::secrets::CODEC
            .get()
            .map(|codec| codec.as_ref() as &dyn AiSecretCodec)
    }
    fn probe_decision(&self, _: &FileSearchSettings) -> Result<(), String> {
        unbuilt("HomeApp::probe_decision");
        Err(NOT_BUILT.into())
    }
    fn mcp_status(&self) -> McpStatus {
        unbuilt("HomeApp::mcp_status");
        not_running()
    }
    fn mcp_apply(&self, _: McpSettings) -> Result<McpStatus, String> {
        unbuilt("HomeApp::mcp_apply");
        Err(NOT_BUILT.into())
    }
    fn mcp_logs(&self) -> Vec<String> {
        unbuilt("HomeApp::mcp_logs");
        Vec::new()
    }
    fn mcp_clear_logs(&self) {
        unbuilt("HomeApp::mcp_clear_logs");
    }
    fn mcp_reveal_log_file(&self) {
        unbuilt("HomeApp::mcp_reveal_log_file");
    }
    fn mcp_log_file(&self) -> Option<PathBuf> {
        unbuilt("HomeApp::mcp_log_file");
        None
    }
}

impl WindowsHost for Unbuilt {
    fn attach(&self, _: Arc<dyn HostEvents>) {
        unbuilt("WindowsHost::attach");
    }
    fn shell_window(&self) -> Option<ShellWindow> {
        unbuilt("WindowsHost::shell_window");
        None
    }
    fn create_shell_window(&self) -> ShellWindow {
        unbuilt("WindowsHost::create_shell_window");
        panic!("WindowsHost::create_shell_window is not built yet")
    }
    fn window_state(&self, _: WindowId) -> WindowState {
        unbuilt("WindowsHost::window_state");
        panic!("WindowsHost::window_state is not built yet")
    }
    fn create_window(&self, _: &WindowOptions) -> shell_ipc_w::tabs::window_snapshot::WindowReservation {
        unbuilt("WindowsHost::create_window");
        panic!("WindowsHost::create_window is not built yet")
    }
    fn window_restore(&self, _: WindowId) {
        unbuilt("WindowsHost::window_restore");
    }
    fn window_show(&self, _: WindowId) {
        unbuilt("WindowsHost::window_show");
    }
    fn window_show_inactive(&self, _: WindowId) {
        unbuilt("WindowsHost::window_show_inactive");
    }
    fn window_focus(&self, _: WindowId) {
        unbuilt("WindowsHost::window_focus");
    }
    fn window_destroy(&self, _: WindowId) {
        unbuilt("WindowsHost::window_destroy");
    }
    fn window_set_title(&self, _: &shell_ipc_w::tabs::window_snapshot::WindowReservation, _: &str) {
        unbuilt("WindowsHost::window_set_title");
        panic!("WindowsHost::window_set_title is not built yet")
    }
    fn window_set_position(&self, _: WindowId, _: f64, _: f64) {
        unbuilt("WindowsHost::window_set_position");
    }
    fn window_set_frame(&self, _: WindowId, _: &main_core::shell::split_layout::Rect) {
        unbuilt("WindowsHost::window_set_frame");
    }
    fn window_work_area(&self, _: WindowId) -> main_core::shell::split_layout::Rect {
        unbuilt("WindowsHost::window_work_area");
        panic!("WindowsHost::window_work_area is not built yet")
    }
    fn add_child_view(&self, _: WindowId, _: ViewId) {
        unbuilt("WindowsHost::add_child_view");
    }
    fn remove_child_view(&self, _: WindowId, _: ViewId) {
        unbuilt("WindowsHost::remove_child_view");
    }
    fn view_destroyed(&self, _: ViewId) -> bool {
        unbuilt("WindowsHost::view_destroyed");
        false
    }
    fn view_set_visible(&self, _: ViewId, _: bool) {
        unbuilt("WindowsHost::view_set_visible");
    }
    fn view_set_bounds(&self, _: ViewId, _: &main_core::shell::split_layout::Rect) {
        unbuilt("WindowsHost::view_set_bounds");
    }
    fn view_focus(&self, _: ViewId) {
        unbuilt("WindowsHost::view_focus");
    }
    fn view_close(&self, _: ViewId) {
        unbuilt("WindowsHost::view_close");
    }
    fn view_reload(&self, _: ViewId) {
        unbuilt("WindowsHost::view_reload");
    }
    fn view_load_url(&self, _: ViewId, _: &str) {
        unbuilt("WindowsHost::view_load_url");
    }
    fn view_listen(&self, _: ViewId, _: ViewListen) {
        unbuilt("WindowsHost::view_listen");
    }
    fn watch_page_load(&self, _: ViewId, _: u64) {
        unbuilt("WindowsHost::watch_page_load");
    }
    fn create_help_view(&self) -> ViewId {
        unbuilt("WindowsHost::create_help_view");
        panic!("WindowsHost::create_help_view is not built yet")
    }
    fn cursor_point(&self) -> (f64, f64) {
        unbuilt("WindowsHost::cursor_point");
        panic!("WindowsHost::cursor_point is not built yet")
    }
    fn display_work_area(&self, _: (f64, f64)) -> main_core::shell::split_layout::Rect {
        unbuilt("WindowsHost::display_work_area");
        panic!("WindowsHost::display_work_area is not built yet")
    }
    fn set_timer(&self, _: TimerKey, _: f64) {
        unbuilt("WindowsHost::set_timer");
    }
    fn clear_timer(&self, _: TimerKey) {
        unbuilt("WindowsHost::clear_timer");
    }
    fn popup_menu(
        &self,
        _: WindowId,
        _: &[shell_ipc_w::tabs::MenuEntry],
        _: Option<(f64, f64)>,
        _: u64,
    ) {
        unbuilt("WindowsHost::popup_menu");
    }
}

impl TabHooks for Unbuilt {
    fn cleanup_lifetime(&self) -> shell_contract::runtime::CancelToken {
        unbuilt("TabHooks::cleanup_lifetime")
    }
    fn register_generated_path_task(
        &self, _: std::sync::Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
        Err("Generated cleanup is unavailable in Unbuilt".into())
    }
    fn capture_generated_retry_prompt(
        &self, _: shell_ipc_w::tabs::ShellWindow, _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::ipc::SavedPublication,
        _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        _: Vec<shell_contract::runtime::CancelToken>,
    ) -> Result<std::rc::Rc<dyn shell_ipc_w::tabs::host::CapturedGeneratedRetryPrompt>, String> {
        Err("Generated retry prompts are unavailable in Unbuilt".into())
    }
    fn capture_saved_shell(
        &self, _: shell_ipc_w::tabs::ShellWindow, _: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<std::rc::Rc<dyn shell_ipc_w::tabs::host::CapturedSavedShell>, String> {
        Err("Saved shell creation is unavailable in Unbuilt".into())
    }
    fn read_saved_input(
        &self, _: shell_contract::ipc::SavedPublication, _: shell_contract::ipc::NativeSavedPath,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken, _: Vec<shell_contract::runtime::CancelToken>,
        then: Box<dyn FnOnce(Result<shell_contract::ipc::NativeSavedInput, String>)>,
    ) {
        then(Err("Saved input reading is unavailable in Unbuilt".into()));
    }
    fn prepare_reopen_existing_document(
        &self, _: shell_contract::ipc::SavedPublication,
        _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        _: Vec<shell_contract::runtime::CancelToken>,
        then: Box<dyn FnOnce(Result<shell_contract::ipc::NativeDriveReopen, String>)>,
    ) {
        then(Err("Drive reopening is unavailable in Unbuilt".into()));
    }
    fn prepare_activation_ui(
        &self, _: shell_ipc_w::tabs::ShellWindow, _: Option<ViewId>,
        _: &[main_core::shell::tab_manager::TabActivationEffect<ViewId, shell_ipc_w::tabs::host::RetainedSpareBinding>],
    ) -> Result<shell_ipc_w::tabs::PreparedTabUiBatch, String> {
        Err("Activation is unavailable in Unbuilt".into())
    }
    fn prepare_docs_disposal_owned(
        &self, _: ViewId, _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::ipc::NativeRequestOwner, _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
    ) -> Result<std::rc::Rc<dyn shell_ipc_w::tabs::DocsDisposalOperation>, String> {
        Err("Document disposal is unavailable in Unbuilt".into())
    }
    fn docs_query_dirty_captured_many(&self, _: ViewId, _: shell_contract::ipc::NativeRequestOwner,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken, then: shell_contract::editors::Then<bool>) {
        then(Err(shell_contract::IpcError { name: Some("Error".into()), message: "Document dirty query is unavailable in Unbuilt".into() }));
    }
    fn docs_request_close_captured_many(&self, _: ViewId, _: Option<WindowId>, _: shell_contract::ipc::NativeRequestOwner,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken, then: shell_contract::editors::Then<bool>) {
        then(Err(shell_contract::IpcError { name: Some("Error".into()), message: "Document close is unavailable in Unbuilt".into() }));
    }
    fn prepare_activation_publication_owned(
        &self, _: shell_ipc_w::tabs::TabActivationAuthority, _: shell_contract::ipc::NativeRequestOwner,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken, _: shell_contract::hosts::OwnedEffectsHost,
        _: shell_ipc_w::tabs::TabActivationPublicationSnapshot,
    ) -> Result<shell_ipc_w::tabs::PreparedTabActivationPublication, String> {
        Err("Activation publication is unavailable in Unbuilt".into())
    }
    fn prepare_saved_publication_owned(
        &self, _: shell_contract::ipc::NativeRequestOwner, _: shell_contract::ipc::NativeRequestOwner,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken, _: Vec<shell_contract::runtime::CancelToken>, _: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_ipc_w::tabs::SavedPublication, String> {
        Err("Saved publication is unavailable in Unbuilt".into())
    }
    fn prepare_saved_publication_recovery(
        &self, _: shell_contract::ipc::RecoverySeedReceipt, _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        _: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_ipc_w::tabs::SavedPublication, String> {
        Err("Recovery saved publication is unavailable in Unbuilt".into())
    }
    fn prepare_saved_publication_existing_document(
        &self, _: shell_contract::ipc::NativeSaveDestination,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken, _: Vec<shell_contract::runtime::CancelToken>, _: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_ipc_w::tabs::SavedPublication, String> {
        Err("Existing-document saved publication is unavailable in Unbuilt".into())
    }
    fn prepare_saved_publication_saved_document(
        &self, _: shell_contract::ipc::NativeSavedDocument,
        _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        _: Vec<shell_contract::runtime::CancelToken>, _: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_ipc_w::tabs::SavedPublication, String> {
        Err("Saved-document publication is unavailable in Unbuilt".into())
    }
    fn create_view(
        &self, _: &main_core::shell::tab_manager::ViewSpec<'_>,
        done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<ViewId>)>,
    ) {
        done(Err(shell_ipc_w::tabs::host::ViewCreationFailure {
            error: TabError("View construction is unavailable in Unbuilt".into()),
            cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState::Complete,
        }));
    }
    fn create_spare_view_owned(
        &self, _: shell_contract::ipc::NativeInvocationOperation, _: shell_contract::ipc::NativeRequestOwner,
        _: shell_contract::runtime::CancelToken, _: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
        _: shell_ipc_w::tabs::host::SpareActorSlot,
        done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
    ) {
        done(Err(shell_ipc_w::tabs::host::ViewCreationFailure {
            error: TabError("Native spare construction is unavailable in Unbuilt".into()),
            cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState::Complete,
        }));
    }
    fn prepare_view_native_command(
        &self, _: &main_core::shell::tab_manager::ViewSpec<'_>, _: shell_contract::ipc::NativeInvocationOperation,
        _: shell_ipc_w::tabs::host::EditorSeedRollbackStore, _: shell_ipc_w::tabs::host::EditorInitializer,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken,
    ) -> main_core::shell::tab_manager::TabResult<(shell_contract::runtime::RendererGeneration, Box<dyn FnOnce(
        Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
    )>)> {
        Err(TabError("Native view construction is unavailable in Unbuilt".into()))
    }
    fn prepare_view_owned(
        &self, _: &main_core::shell::tab_manager::ViewSpec<'_>,
        _: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
        _: shell_ipc_w::tabs::host::EditorInitializer,
    ) -> main_core::shell::tab_manager::TabResult<(shell_contract::runtime::RendererGeneration, Box<dyn FnOnce(
        shell_contract::ipc::NativeRequestOwner, shell_contract::runtime::CancelToken, shell_contract::runtime::CancelToken,
        Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<shell_contract::ipc::NativeRequestOwner>)>,
    )>)> {
        Err(TabError("View construction is unavailable in Unbuilt".into()))
    }
    fn prepare_generated_view_owned(
        &self, _: &main_core::shell::tab_manager::ViewSpec<'_>,
        _: shell_contract::ipc::NativeRequestOwner,
        _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        _: Option<main_core::shell::tab_manager::EditorSeed>,
        _: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        _: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> main_core::shell::tab_manager::TabResult<shell_ipc_w::tabs::host::PreparedGeneratedView> {
        Err(TabError("Generated view construction is unavailable in Unbuilt".into()))
    }
    fn prepare_initialized_spare_owned(
        &self, _: ViewId, _: &shell_contract::runtime::RendererGeneration,
        _: shell_ipc_w::tabs::host::RetainedSpareBinding,
        _: shell_contract::ipc::NativeRequestOwner,
        _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        _: Option<main_core::shell::tab_manager::EditorSeed>,
        _: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        _: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> main_core::shell::tab_manager::TabResult<shell_ipc_w::tabs::host::PreparedGeneratedView> {
        Err(TabError("Initialized spare construction is unavailable in Unbuilt".into()))
    }
    fn commit_generated_seed_owned(
        &self, _: ViewId, _: Option<main_core::shell::tab_manager::EditorSeed>,
        _: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        _: &shell_contract::ipc::NativeRequestOwner, _: &[shell_contract::ipc::NativeRequestOwner],
        _: &shell_contract::runtime::CancelToken, _: &shell_contract::runtime::CancelToken,
        _: &shell_contract::runtime::RendererGeneration,
        _: &shell_contract::ipc::NativeClosureReservation,
        _: &shell_contract::ipc::NativeBrowserOwner,
        _: &shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> main_core::shell::tab_manager::TabResult<()> {
        Err(TabError("Generated seed publication is unavailable in Unbuilt".into()))
    }
    fn prepare_spare_editor_seed(
        &self, _: ViewId, _: Option<main_core::shell::tab_manager::EditorSeed>,
        _: Option<String>, _: Option<String>, _: bool, _: &shell_contract::ipc::NativeRequestOwner,
        _: &shell_ipc_w::tabs::host::RetainedSpareBinding,
        _: &shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> main_core::shell::tab_manager::TabResult<Box<dyn shell_ipc_w::tabs::host::SpareSeedPlan>> {
        Err(TabError("Spare seed preparation is unavailable in Unbuilt".into()))
    }
    fn commit_editor_seed_owned(
        &self, _: ViewId, _: Option<main_core::shell::tab_manager::EditorSeed>, _: Option<String>, _: Option<String>, _: Option<&shell_contract::ipc::NativeRequestOwner>,
        _: &shell_contract::ipc::NativeRequestOwner,
        _: &shell_ipc_w::tabs::host::EditorSeedRollbackStore,
        _: &shell_contract::runtime::RendererGeneration, _: &shell_contract::ipc::NativeClosureReservation, _: &shell_contract::ipc::NativeBrowserOwner, _: &shell_contract::runtime::CancelToken,
        _: &shell_contract::runtime::CancelToken,
    ) -> main_core::shell::tab_manager::TabResult<()> {
        Err(TabError("Editor seed publication is unavailable in Unbuilt".into()))
    }
    fn commit_native_editor_seed(
        &self, _: &shell_contract::ipc::NativeInvocationOperation, _: &shell_contract::ipc::NativeEditorCommand,
        _: ViewId, _: Option<main_core::shell::tab_manager::EditorSeed>, _: Option<String>, _: Option<String>,
        _: Option<&shell_contract::ipc::NativeRequestOwner>, _: &shell_ipc_w::tabs::host::EditorSeedRollbackStore,
        _: &shell_contract::runtime::RendererGeneration, _: &shell_contract::ipc::NativeClosureReservation, _: &shell_contract::ipc::NativeBrowserOwner, _: &shell_contract::runtime::CancelToken,
        _: &shell_contract::runtime::CancelToken,
    ) -> main_core::shell::tab_manager::TabResult<()> {
        Err(TabError("Native editor initialization is unavailable in Unbuilt".into()))
    }
    fn reveal_written_export_owned(
        &self,_:std::path::PathBuf,
        _:shell_contract::ipc::NativeRequestOwner,_:shell_contract::ipc::NativeRequestOwner,
        _:shell_contract::runtime::CancelToken,_:shell_contract::runtime::CancelToken,
        done:Box<dyn FnOnce(Result<(),String>)>,
    ) {
        done(Err("Export reveal is unavailable in Unbuilt".into()));
    }
    fn reveal_written_export_existing_document(
        &self, _: shell_contract::ipc::NativeSaveDestination, _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        _: Vec<shell_contract::runtime::CancelToken>, done: Box<dyn FnOnce(Result<(), String>)>,
    ) { done(Err("Native export reveal is unavailable in Unbuilt".into())); }
    fn prepare_reopen_owned(
        &self,path:String,owner:shell_contract::ipc::NativeRequestOwner,
        _:shell_contract::ipc::NativeRequestOwner,cancel:shell_contract::runtime::CancelToken,
        _:shell_contract::runtime::CancelToken,
        done:Box<dyn FnOnce(Result<(String,Option<String>,shell_contract::ipc::NativeRequestOwner,shell_contract::runtime::CancelToken),String>)>,
    ) {
        drop((path,owner,cancel));
        done(Err("Drive reopen is unavailable in Unbuilt".into()));
    }
    fn open_opener_owned(
        &self,_:&Opener,_:&UnloadedTabOpenContext,
        _:shell_contract::ipc::NativeRequestOwner,_:shell_contract::runtime::CancelToken,
        _:shell_contract::runtime::CancelToken,
        done:Box<dyn FnOnce(Result<shell_ipc_w::tabs::OwnedLoadedTabView,String>)>,
    ) { done(Err("Recovery opener is unavailable in Unbuilt".into())); }
    fn open_opener_startup(
        &self, _: &Opener, _: &UnloadedTabOpenContext, _: shell_ipc_w::tabs::StartupRecoveryConstraint,
        done: Box<dyn FnOnce(Result<shell_ipc_w::tabs::OwnedLoadedTabView, String>)>,
    ) { done(Err("Recovery opener is unavailable in Unbuilt".into())); }
    fn open_opener_native_command(
        &self, _: &Opener, _: &UnloadedTabOpenContext, _: shell_contract::ipc::NativeInvocationOperation,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<shell_ipc_w::tabs::OwnedLoadedTabView, String>)>,
    ) { done(Err("Native command recovery opener is unavailable in Unbuilt".into())); }
    fn opener_needs_user_owned(
        &self,_:&Opener,_:&UnloadedTabOpenContext,_:shell_contract::ipc::NativeRequestOwner,
        _:shell_contract::runtime::CancelToken,_:shell_contract::runtime::CancelToken,
        done:Box<dyn FnOnce(Result<bool,String>)>,
    ) { done(Err("Recovery query is unavailable in Unbuilt".into())); }
    fn reveal_captured_many(
        &self, _: shell_ipc_w::tabs::TabActivationAuthority,
        _: shell_contract::ipc::NativeRequestOwner, _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        done: shell_ipc_w::tabs::TabUiDone,
    ) { done(Err("Captured reveal is unavailable in Unbuilt".into())); }
    fn adopt_recovery_view_owned(
        &self, _: shell_ipc_w::tabs::ShellWindow, _: shell_ipc_w::tabs::TabActivationAuthority,
        _: shell_contract::ipc::NativeRequestOwner, _: Vec<shell_contract::ipc::NativeRequestOwner>,
        _: shell_contract::runtime::CancelToken, _: shell_contract::runtime::CancelToken,
        done: shell_ipc_w::tabs::TabUiDone,
    ) { done(Err("Recovery adoption is unavailable in Unbuilt".into())); }
    fn rollback_generated_seed_owned(&self, _: ViewId, _: &shell_contract::runtime::RendererGeneration) {
        panic!("Generated seed cleanup is unavailable in Unbuilt")
    }
    fn sheets_pending_edit_count(&self, _: i64) -> f64 {
        unbuilt("TabHooks::sheets_pending_edit_count");
        0.0
    }
    fn family_is_dirty(&self, _: TabKind, _: i64) -> main_core::shell::tab_manager::TabResult<bool> {
        unbuilt("TabHooks::family_is_dirty");
        Err(TabError(NOT_BUILT.to_owned()))
    }
    fn hwp_memo_modal_open(&self, _: i64) -> bool {
        unbuilt("TabHooks::hwp_memo_modal_open");
        false
    }
    fn docs_query_dirty(&self, _: ViewId, _: Box<dyn FnOnce(Result<bool, IpcError>)>) {
        unbuilt("TabHooks::docs_query_dirty");
    }
    fn set_active_docs_resolver(&self, _: Option<ViewId>, _: bool) {
        unbuilt("TabHooks::set_active_docs_resolver");
    }
    fn set_active_sheets_web_contents(&self, _: Option<ViewId>) {
        unbuilt("TabHooks::set_active_sheets_web_contents");
    }
    fn set_active_slides_web_contents(&self, _: ViewId) {
        unbuilt("TabHooks::set_active_slides_web_contents");
    }
    fn apply_menu_for(&self, kind: TabKind) {
        super::menu::apply_menu(kind);
    }
    fn teardown_docs_renderer(&self, _: ViewId) {
        unbuilt("TabHooks::teardown_docs_renderer");
    }
    fn release_closed_renderer(&self, _: ViewId) {
        unbuilt("TabHooks::release_closed_renderer");
    }
    fn mark_docs_new_blank(&self, _: i64) {
        unbuilt("TabHooks::mark_docs_new_blank");
    }
    fn queue_docs_ai_content(&self, _: i64, _: &serde_json::Value) -> main_core::shell::tab_manager::TabResult<()> {
        Err(TabError("AI document content is unavailable in Unbuilt".into()))
    }
    fn queue_docs_copy(&self, _: i64, _: &str, _: &str) {
        unbuilt("TabHooks::queue_docs_copy");
    }
    fn set_sheets_new_blank(&self) {
        unbuilt("TabHooks::set_sheets_new_blank");
    }
    fn queue_workbook_for_view(&self, _: ViewId, _: &str) {
        unbuilt("TabHooks::queue_workbook_for_view");
    }
    fn nudge_queued_workbook(&self, _: ViewId) {
        unbuilt("TabHooks::nudge_queued_workbook");
    }
    fn mark_pdf_untitled_copy(&self, _: &str) {
        unbuilt("TabHooks::mark_pdf_untitled_copy");
    }
    fn clear_pdf_dirty(&self, _: i64) {
        unbuilt("TabHooks::clear_pdf_dirty");
    }
    fn queue_markdown_seed_for(&self, _: ViewId, _: &str) {
        unbuilt("TabHooks::queue_markdown_seed_for");
    }
    fn queue_html_seed_for(&self, _: ViewId, _: &str) {
        unbuilt("TabHooks::queue_html_seed_for");
    }
    fn queue_mindmap_seed_for(&self, _: ViewId, _: &[u8]) {
        unbuilt("TabHooks::queue_mindmap_seed_for");
    }
    fn queue_slides_seed_for(&self, _: ViewId, _: &[u8], _: &str) {
        unbuilt("TabHooks::queue_slides_seed_for");
    }
    fn queue_generate_handoff_for(&self, _: ViewId, _: &serde_json::Value) {
        unbuilt("TabHooks::queue_generate_handoff_for");
    }
    fn queue_converted_content(&self, _: ViewId, _: &serde_json::Value) {
        unbuilt("TabHooks::queue_converted_content");
    }
    fn drop_generate_handoff(&self, _: i64) {
        unbuilt("TabHooks::drop_generate_handoff");
    }
    fn diagram_tab_identity(&self, _: i64) -> Option<DiagramIdentity> {
        unbuilt("TabHooks::diagram_tab_identity");
        None
    }
    fn opener_open(
        &self,
        _: &Opener,
        _: &UnloadedTabOpenContext,
    ) -> Result<LoadedTabView<ViewId>, String> {
        unbuilt("TabHooks::opener_open");
        Err(NOT_BUILT.into())
    }
    fn opener_needs_user(
        &self,
        _: &Opener,
        _: &UnloadedTabOpenContext,
    ) -> Result<bool, String> {
        unbuilt("TabHooks::opener_needs_user");
        Err(NOT_BUILT.into())
    }
    fn reset_sheets_shutting_down(&self) {
        unbuilt("TabHooks::reset_sheets_shutting_down");
    }
    fn cancel_update_restart(&self) {
        unbuilt("TabHooks::cancel_update_restart");
    }
    fn resume_discarded_copies(&self) {
        unbuilt("TabHooks::resume_discarded_copies");
    }
    fn pdf_close_cancelled(&self, _: ViewId) {
        unbuilt("TabHooks::pdf_close_cancelled");
    }
    fn ui_scale(&self) -> f64 {
        crate::ui_scale::current()
    }
    fn no_spare_view_env(&self) -> bool {
        unbuilt("TabHooks::no_spare_view_env");
        false
    }
    fn renderer_dev_url(&self) -> Option<String> {
        unbuilt("TabHooks::renderer_dev_url");
        None
    }
    fn tm(&self, _: &str) -> String {
        unbuilt("TabHooks::tm");
        String::new()
    }
    fn tm_params(&self, _: &str, _: &[(&str, &str)]) -> String {
        unbuilt("TabHooks::tm_params");
        String::new()
    }
    fn random_uuid(&self) -> String {
        unbuilt("TabHooks::random_uuid");
        String::new()
    }
    fn warn(&self, _: &str, _: &str) {
        unbuilt("TabHooks::warn");
    }
    fn tabs_changed(&self, _: &TabsChanged) {
        unbuilt("TabHooks::tabs_changed");
    }
    fn split_changed(&self, _: Option<&SplitViewState>) {
        unbuilt("TabHooks::split_changed");
    }
    fn detached_changed(&self) {
        unbuilt("TabHooks::detached_changed");
    }
    fn app_menu_popup(&self) -> Option<Vec<shell_ipc_w::tabs::MenuEntry>> {
        super::menu::app_menu_popup()
    }
    fn app_menu_activate(&self, id: u32, cx: shell_contract::ipc::CallContext) {
        super::menu::app_menu_activate(id, cx);
    }
    fn visible_home_kinds(&self) -> Vec<String> {
        shell_ipc_w::home_settings::visible_home_kinds(
            &crate::paths::ShellPaths::new()
                .settings_file()
                .to_string_lossy(),
        )
    }
    fn drive_reopen(&self, _: &str) -> bool {
        unbuilt("TabHooks::drive_reopen");
        false
    }
    fn record_recent_file(&self, _: &str) {
        unbuilt("TabHooks::record_recent_file");
    }
    fn record_recent_file_native(
        &self, _: String, _: shell_contract::ipc::NativeInvocationOperation,
        _: shell_contract::ipc::NativeEditorCommand, done: Box<dyn FnOnce(Result<(), String>)>,
    ) { done(Err("Native recent-file publication is unavailable in Unbuilt".into())); }
    fn notify_unsupported_file(&self, _: &str) {
        unbuilt("TabHooks::notify_unsupported_file");
    }
    fn notify_unsupported_file_native(
        &self, _: String, _: shell_contract::ipc::NativeInvocationOperation, _: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) { done(Err("Native unsupported-file warning is unavailable in Unbuilt".into())); }
    fn show_app_warning(&self, _: &str, _: Option<&str>) {
        unbuilt("TabHooks::show_app_warning");
    }
    fn show_app_warning_owned(
        &self, _: String, _: Option<String>, _: shell_contract::ipc::NativeRequestOwner,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken, done: Box<dyn FnOnce(Result<(), String>)>,
    ) { done(Err("Owned warning is unavailable in Unbuilt".into())); }
    fn show_app_warning_native(
        &self, _: String, _: Option<String>, _: shell_contract::ipc::NativeInvocationOperation,
        _: shell_contract::ipc::NativeEditorCommand, done: Box<dyn FnOnce(Result<(), String>)>,
    ) { done(Err("Native warning is unavailable in Unbuilt".into())); }
    fn diagram_open_refusal(&self, _: &str, then: Box<dyn FnOnce(Option<String>)>) {
        unbuilt("TabHooks::diagram_open_refusal");
        then(None);
    }
    fn diagram_open_refusal_owned(
        &self, _: String, _: shell_contract::ipc::NativeRequestOwner,
        _: Vec<shell_contract::ipc::NativeRequestOwner>, _: shell_contract::runtime::CancelToken,
        _: shell_contract::runtime::CancelToken, done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    ) { done(Err("Owned diagram refusal check is unavailable in Unbuilt".into())); }
    fn diagram_open_refusal_native(
        &self, _: String, _: shell_contract::ipc::NativeInvocationOperation, _: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    ) { done(Err("Native diagram refusal check is unavailable in Unbuilt".into())); }
    fn start_queued_workbook_nudge(&self) {
        unbuilt("TabHooks::start_queued_workbook_nudge");
    }
    fn default_save_dir(&self) -> String {
        unbuilt("TabHooks::default_save_dir");
        String::new()
    }
    fn blank_xlsx(&self) -> Result<Vec<u8>, String> {
        unbuilt("TabHooks::blank_xlsx");
        Err(NOT_BUILT.into())
    }
    fn blank_pdf(&self) -> Vec<u8> {
        unbuilt("TabHooks::blank_pdf");
        Vec::new()
    }
    fn unique_path_in(&self, _: &str, _: &str) -> String {
        unbuilt("TabHooks::unique_path_in");
        String::new()
    }
    fn sheets_suggested_path_taken(&self, _: &str) -> bool {
        unbuilt("TabHooks::sheets_suggested_path_taken");
        false
    }
    fn mark_sheets_unsaved_new(&self, _: &str, _: &str, _: &str) {
        unbuilt("TabHooks::mark_sheets_unsaved_new");
    }
    fn unmark_sheets_unsaved_new(&self, _: &str) {
        unbuilt("TabHooks::unmark_sheets_unsaved_new");
    }
    fn mark_sheets_untitled_path(&self, _: &str) {
        unbuilt("TabHooks::mark_sheets_untitled_path");
    }
    fn mark_pdf_untitled_path(&self, _: &str) {
        unbuilt("TabHooks::mark_pdf_untitled_path");
    }
    fn show_error_dialog(&self, _: &str, _: &str) {
        unbuilt("TabHooks::show_error_dialog");
    }
    fn open_agent_window(&self) {
        unbuilt("TabHooks::open_agent_window");
    }
    fn drop_skill_file(&self, _: &str, _: Box<dyn FnOnce(bool)>) {
        unbuilt("TabHooks::drop_skill_file");
        panic!("TabHooks::drop_skill_file is not built yet");
    }
    fn drop_no_skills_found(&self) {
        unbuilt("TabHooks::drop_no_skills_found");
    }
    fn is_inside_root(&self, _: &str, _: &str) -> bool {
        unbuilt("TabHooks::is_inside_root");
        false
    }
    fn open_file_via_dialog(&self) {
        unbuilt("TabHooks::open_file_via_dialog");
    }
    fn tab_document_action(&self, _: TabKind, _: ViewId, _: &str, _: Option<&str>, _: DocAction) {
        unbuilt("TabHooks::tab_document_action");
    }
}

// ---- the project modules (`shell_ipc_w::projects`) ----

/// `Date`'s local getters of a time value: the civil date in UTC. The local
/// time zone is not read yet, so a project guide's day stamps are UTC days.
fn utc_date(at_ms: f64) -> LocalDateParts {
    let secs = (at_ms / 1000.0).floor() as i64;
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    LocalDateParts {
        full_year: year,
        month: month - 1,
        date,
        hours: rest / 3_600,
        minutes: rest % 3_600 / 60,
        seconds: rest % 60,
    }
}

/// `Intl.NumberFormat(lang).format(count)` for English and Korean, which
/// group by threes with commas and show up to three decimals
fn grouped(count: f64) -> String {
    if !count.is_finite() {
        return if count.is_nan() {
            "NaN".into()
        } else if count > 0.0 {
            "∞".into()
        } else {
            "-∞".into()
        };
    }
    let text = format!("{:.3}", count.abs());
    let (int, frac) = text.split_once('.').unwrap_or((&text, ""));
    let frac = frac.trim_end_matches('0');
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    if count < 0.0 && out.chars().any(|c| c != '0' && c != '.' && c != ',') {
        out.insert(0, '-');
    }
    out
}

/// `crypto.randomBytes(n)` for ids: the process's random hasher keys (seeded
/// by the operating system) over a counter and the clock. Not for secrets.
fn random_bytes(n: usize) -> Vec<u8> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut out = Vec::with_capacity(n + 8);
    while out.len() < n {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        out.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    out.truncate(n);
    out
}

/// Real: the random source, the home folder, the number format (English and
/// Korean) and `console.warn`. The date is the UTC day, and what needs the
/// page (focus, navigation, timers) and the Windows listing are logged stand-ins.
impl ProjectsHost for Unbuilt {
    fn with_request_owner(
        &self,
        _: &shell_contract::ipc::NativeRequestOwner,
        _: &shell_contract::runtime::CancelToken,
        _: &mut dyn FnMut() -> Result<(), shell_contract::IpcError>,
    ) -> Result<(), shell_contract::IpcError> {
        Err(shell_contract::IpcError {
            name: Some("Error".into()),
            message: "Renderer document authentication is not ready.".into(),
        })
    }
    fn attach(&self, _: Arc<dyn PageEvents>) {
        unbuilt("ProjectsHost::attach");
    }
    fn track(&self, _: ViewId) {
        unbuilt("ProjectsHost::track");
    }
    fn page_destroyed(&self, _: ViewId) -> bool {
        unbuilt("ProjectsHost::page_destroyed");
        false
    }
    fn watch_return(&self, _: ViewId) {
        unbuilt("ProjectsHost::watch_return");
    }
    fn stop_return(&self, _: ViewId) {
        unbuilt("ProjectsHost::stop_return");
    }
    fn set_timer(&self, _: ProjectTimer, _: f64) {
        unbuilt("ProjectsHost::set_timer");
    }
    fn clear_timer(&self, _: ProjectTimer) {
        unbuilt("ProjectsHost::clear_timer");
    }
    fn random_bytes(&self, n: usize) -> Vec<u8> {
        random_bytes(n)
    }
    fn local_date(&self, at_ms: f64) -> LocalDateParts {
        unbuilt("ProjectsHost::local_date (UTC, not the local zone)");
        utc_date(at_ms)
    }
    fn os_home_dir(&self) -> String {
        dirs::home_dir().map_or_else(String::new, |p| p.to_string_lossy().into_owned())
    }
    fn format_number(&self, count: f64, _: Lang) -> Result<String, String> {
        Ok(grouped(count))
    }
    fn format_date_time(&self, _: f64, _: Lang) -> Result<String, String> {
        unbuilt("ProjectsHost::format_date_time");
        Err(NOT_BUILT.into())
    }
    fn run_listing(
        &self,
        _: &str,
        _: &[String],
        _: &str,
        _: &[(String, String)],
        _: f64,
        _: &str,
    ) -> Result<String, String> {
        unbuilt("ProjectsHost::run_listing");
        Err(NOT_BUILT.into())
    }
    fn warn(&self, label: &str, detail: Option<&str>) {
        match detail {
            Some(detail) => eprintln!("shell: {label} {detail}"),
            None => eprintln!("shell: {label}"),
        }
    }
}

impl ProjectFiles for Unbuilt {
    fn search(&self, _: &str, _: &mut IndexSearchOptions<'_>) -> Result<IndexSearchResult, Thrown> {
        unbuilt("ProjectFiles::search");
        Err(Thrown(NOT_BUILT.into()))
    }
    fn paths_indexed_as_on_disk(&self, _: &[String]) -> Result<Vec<String>, Thrown> {
        unbuilt("ProjectFiles::paths_indexed_as_on_disk");
        Err(Thrown(NOT_BUILT.into()))
    }
    fn read_page(&self, _: &str, _: f64) -> Result<KbReadPage, Thrown> {
        unbuilt("ProjectFiles::read_page");
        Err(Thrown(NOT_BUILT.into()))
    }
    fn open_files_async(&self, done: Box<dyn FnOnce(Result<Vec<OpenFileState>, OpenFilesSourceFailed>)>) {
        unbuilt("ProjectFiles::open_files_async");
        done(Err(OpenFilesSourceFailed));
    }
}

impl LegacyProjectStore for Unbuilt {
    fn legacy_chat_for(&self, _: &str) -> Result<Option<LegacyDocumentChat>, String> {
        unbuilt("LegacyProjectStore::legacy_chat_for");
        Ok(None)
    }
    fn ensure_default_project(&self) -> Result<(), String> {
        unbuilt("LegacyProjectStore::ensure_default_project");
        Err(NOT_BUILT.into())
    }
    fn resolve_chat_for_file(&self, _: &Value) -> Result<Value, String> {
        unbuilt("LegacyProjectStore::resolve_chat_for_file");
        Err(NOT_BUILT.into())
    }
    fn lookup_chat_for_file(&self, _: &Value) -> Result<Value, String> {
        unbuilt("LegacyProjectStore::lookup_chat_for_file");
        Err(NOT_BUILT.into())
    }
    fn append_chat_message(&self, _: &Value, _: &Value, _: &Value) -> Result<(), String> {
        unbuilt("LegacyProjectStore::append_chat_message");
        Err(NOT_BUILT.into())
    }
    fn load_chat(&self, _: &Value, _: &Value, _: &Value) -> Result<Value, String> {
        unbuilt("LegacyProjectStore::load_chat");
        Err(NOT_BUILT.into())
    }
    fn rebind_chat_to_file(&self, _: &Value, _: &Value, _: &Value) -> Result<Value, String> {
        unbuilt("LegacyProjectStore::rebind_chat_to_file");
        Err(NOT_BUILT.into())
    }
    fn rebind_chat(&self, _: &Value, _: &Value, _: &Value) -> Result<(), String> {
        unbuilt("LegacyProjectStore::rebind_chat");
        Err(NOT_BUILT.into())
    }
    fn sheets_session_path(&self, _: ViewId, _: &Value) -> Option<String> {
        unbuilt("LegacyProjectStore::sheets_session_path");
        None
    }
    fn headless(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_civil_date_and_the_number_format() {
        // 2026-11-14 08:09:10 UTC
        let d = utc_date(1_794_643_750_000.0);
        assert_eq!(
            (d.full_year, d.month, d.date, d.hours, d.minutes, d.seconds),
            (2026, 10, 14, 8, 9, 10)
        );
        let epoch = utc_date(0.0);
        assert_eq!((epoch.full_year, epoch.month, epoch.date), (1970, 0, 1));
        assert_eq!(grouped(1234567.0), "1,234,567");
        assert_eq!(grouped(0.5), "0.5");
        assert_eq!(grouped(-1234.25), "-1,234.25");
        assert_eq!(grouped(999.0), "999");
        assert_eq!(random_bytes(20).len(), 20);
        assert_ne!(random_bytes(16), random_bytes(16));
    }
}
