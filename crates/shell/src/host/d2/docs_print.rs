//! Sandboxed print surface for a generated Word document.

use shell_ipc_m::hosts::{PrintProfile, PrintWindowSpec};
use crate::host::HostContext;

struct WorkDir(std::path::PathBuf);
impl Drop for WorkDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

pub(super) fn html_to_pdf(cx: &HostContext, html: String) -> Result<Vec<u8>, String> {
    render(html, |spec| Ok(crate::host::print::window(cx.app.clone(), cx.ui.clone(), cx.views.clone(), spec)))
}

pub(super) fn html_to_pdf_owned(
    cx: &HostContext, html: String, origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
) -> Result<Vec<u8>, String> {
    if cx.ui.is_current() || crate::host::w::on_core_local(|_| {}) {
        return Err("AI document PDF rendering requires a worker".into());
    }
    render(html, |spec| crate::host::print::window_owned(cx, spec, origin, destination, cancel, pending))
}

fn render(
    html: String, create: impl FnOnce(PrintWindowSpec) -> Result<Box<dyn shell_ipc_m::hosts::PrintSurface>, String>,
) -> Result<Vec<u8>, String> {
    let dir = std::env::temp_dir().join(format!("seuoffice-ai-doc-{}", jscompat::random_uuid()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
    builder.create(&dir).map_err(|error| error.to_string())?;
    let dir = WorkDir(dir);
    let path = dir.0.join("print.html");
    std::fs::write(&path, html).map_err(|error| error.to_string())?;
    let mut surface = create(PrintWindowSpec { profile: PrintProfile::Static, width: None, height: None, parent: None, dialog_chrome: false })?;
    surface.load_file(path.to_str().ok_or("print page path is not UTF-8")?)?;
    surface.print_to_pdf(&serde_json::json!({
        "pageSize": "A4", "printBackground": true,
        "margins": { "top": 0.6, "bottom": 0.6, "left": 0.6, "right": 0.6 }
    }))
}
