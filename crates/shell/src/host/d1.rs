//! Production hosts shared by the D1 document editors.

use std::sync::Arc;

use main_core::packages::i18n::Lang;
use shell_contract::Hosts;
use shell_ipc_d1::host::{Locale, LocaleHost};
use shell_ipc_w::editor_files::UiLanguage;
use tauri::Manager;

use super::{HostContext, w_files::ShellLanguage};

pub(crate) mod diagram;
pub(crate) mod diagram_headless;
mod file_parse;
mod page_renderer;
pub(crate) mod save_guard;
mod sheets_shell;
pub(crate) mod sheets_headless;
pub(crate) mod shells;
pub(crate) mod slides_engine;
pub(crate) mod slides_fonts;
mod slides_generate;
pub(crate) mod slides_media;
pub(crate) mod slides_shell;
mod slides_templates;
mod sidecar;
mod workbook;
mod workbook_crypto;
mod workbook_csv;
mod workbook_csv_xlsx;
mod workbook_legacy;
mod workbook_manifest;
mod workbook_pdf;
mod workbook_promote;
mod workbook_readers;
mod workbook_redactions;
mod workbook_rpc;
mod workbook_save;
mod workbook_save_planner;
mod workbook_schema;
mod workbook_temp;
mod workbook_xml;
mod xmind;

struct ShellLocale(ShellLanguage);

impl Locale for ShellLocale {
    fn ui_lang(&self) -> Lang {
        self.0.lang()
    }
}

struct WorkbookRuntime(Arc<workbook::Engine>);

impl Drop for WorkbookRuntime {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

pub(super) fn install_workbook(hosts: &mut Hosts, cx: &HostContext) -> Result<(), String> {
    let engine = Arc::new(workbook::Engine::new(cx).map_err(|error| error.message)?);
    let host: shell_ipc_d1::sheets::engine::WorkbookEngineHost = engine.clone();
    cx.app.manage(WorkbookRuntime(engine));
    hosts.insert(host);
    sheets_shell::install(hosts, cx)?;
    Ok(())
}

pub(super) fn on_run_event(app: &tauri::AppHandle, event: &tauri::RunEvent) {
    if matches!(event, tauri::RunEvent::Exit)
        && let Some(runtime) = app.try_state::<WorkbookRuntime>()
    {
        runtime.0.shutdown();
    }
}

pub fn install(hosts: &mut Hosts, cx: &HostContext) -> Result<(), String> {
    let locale: LocaleHost = Arc::new(ShellLocale(ShellLanguage(cx.ui.clone())));
    hosts.insert(locale);
    let parser: shell_ipc_d1::mindmap::FileParseHost = Arc::new(file_parse::Parser);
    hosts.insert(parser);
    let xmind: shell_ipc_d1::mindmap::XmindEngineHost = Arc::new(xmind::Engine);
    hosts.insert(xmind);
    let diagram: shell_ipc_d1::diagram::DiagramEngineHost = Arc::new(diagram::Engine);
    hosts.insert(diagram);
    let renderer: shell_ipc_d1::mindmap::PageRendererHost = Arc::new(page_renderer::Renderer(cx.clone()));
    hosts.insert(renderer);
    shells::install_mindmap(hosts);
    shells::install_diagram(hosts, cx);
    slides_shell::install(hosts, cx)?;
    Ok(())
}
