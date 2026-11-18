//! Hidden print surface for mind map PDF export.

use shell_ipc_d1::mindmap::PageRenderer;
use shell_ipc_m::hosts::{PrintProfile, PrintWindowSpec};

use crate::host::HostContext;

pub(super) struct Renderer(pub HostContext);

struct WorkDir(std::path::PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

impl PageRenderer for Renderer {
    fn page_to_pdf(&self, html: &str) -> Result<Vec<u8>, String> {
        self.render_page(html, |spec| Ok(super::super::print::window(self.0.app.clone(), self.0.ui.clone(), self.0.views.clone(), spec)))
    }

    fn page_to_pdf_owned(&self, html: &str, origin: &shell_contract::ipc::NativeRequestOwner, destination: &shell_contract::ipc::NativeRequestOwner, cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken, participants: &[shell_contract::ipc::NativeRequestOwner], additional: &[shell_contract::runtime::CancelToken]) -> Result<Vec<u8>, String> {
        self.render_page(html, |spec| super::super::print::window_owned_many_extra(&self.0, spec, origin, destination, participants, cancel, pending, additional))
    }

    fn page_to_pdf_native(&self, html: &str, binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner], cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken, additional: &[shell_contract::runtime::CancelToken]) -> Result<Vec<u8>, String> {
        self.render_page(html, |spec| super::super::print::window_existing_document(&self.0, spec, binding, participants, cancel, pending, additional))
    }
}

impl Renderer {
    fn render_page(&self, html: &str, create: impl FnOnce(PrintWindowSpec) -> Result<Box<dyn shell_ipc_m::hosts::PrintSurface>, String>) -> Result<Vec<u8>, String> {
        let path = std::env::temp_dir().join(format!("seuoffice-mindmap-pdf-{}", jscompat::random_uuid()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|error| error.to_string())?;
        let directory = WorkDir(path);
        let page = directory.0.join("map.html");
        std::fs::write(&page, html).map_err(|error| error.to_string())?;
        let path = page.to_str().ok_or("print page path is not UTF-8")?;
        let mut surface = create(PrintWindowSpec {
            profile: PrintProfile::MindmapExport,
            width: Some(1123.0), height: Some(794.0), parent: None, dialog_chrome: false,
        })?;
        surface.load_file(path)?;
        surface.print_to_pdf(&serde_json::json!({
            "landscape": true,
            "pageSize": "A4",
            "printBackground": true,
            "preferCSSPageSize": true,
            "margins": { "top": 0, "bottom": 0, "left": 0, "right": 0 }
        }))
    }
}
