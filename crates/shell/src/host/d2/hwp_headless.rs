use main_core::packages::electron_utils::headless_export::HeadlessExportRequest;
use main_core::shell::headless_export::HeadlessExportProduct;
use shell_ipc_d2::hwp::{HEADLESS_TIMEOUT_MS, HwpEditors};

use crate::host::HostContext;

pub(crate) fn export_headless(
    _cx: &HostContext,
    input: &str,
    out: &str,
    format: &str,
    _request: &HeadlessExportRequest,
    operation: &shell_contract::ipc::NativeInvocationOperation,
    done: Box<dyn FnOnce(Result<Option<HeadlessExportProduct>, String>) + Send>,
) -> Result<(), String> {
    if format != "pdf" { return Err(format!("unsupported HWP export format: {format}")); }
    match operation.selection() {
        shell_contract::ipc::NativeInvocationSelection::HeadlessExport { surface, input: selected_input, output, format: selected_format, page, ops, batch }
            if *surface == shell_contract::Surface::Hwp
                && selected_input == std::path::Path::new(input)
                && output == std::path::Path::new(out)
                && selected_format == format && *page == _request.page
                && ops.as_deref() == _request.ops.as_deref()
                && batch.as_deref() == _request.batch => {}
        _ => return Err("HWP headless request does not match its native invocation".into()),
    }
    let mut done = Some(done);
    let mut started = false;
    let on_core = crate::host::w::on_core_local(|provided| {
        let Some(editors) = provided.all::<HwpEditors>().into_iter().next() else { return; };
        let Some(done) = done.take() else { return; };
        started = true;
        editors.export_hwp_pdf_headless_native(input, out, HEADLESS_TIMEOUT_MS, operation.clone(), Box::new(move |result| {
            done(result.map(|()| None).map_err(|error| error.message));
        }));
    });
    if !on_core { return Err("HWP headless export requires the registered core thread".into()); }
    if !started { return Err("HWP headless editor is not registered".into()); }
    Ok(())
}
