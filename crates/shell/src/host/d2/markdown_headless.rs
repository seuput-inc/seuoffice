use main_core::packages::electron_utils::headless_export::HeadlessExportRequest;

use crate::host::HostContext;
use super::html_headless::{Done, TextExport, export_text};

pub(crate) fn export_headless(
    cx: &HostContext, input: &str, out: &str, format: &str, request: &HeadlessExportRequest, operation: &shell_contract::ipc::NativeInvocationOperation, done: Done,
) -> Result<(), String> {
    if format != "pdf" { return Err(format!("unsupported Markdown export format: {format}")); }
    export_text(cx, input, out, format, request, operation, TextExport::MarkdownPdf, done)
}
