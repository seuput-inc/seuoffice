//! Workbook PDF passes from apps/sheets/src/main/pdf-export.ts.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use main_core::sheets::pdf_page_variants::{VariantFlags, even_page_ranges, stitch_plan};
use serde_json::{Map, Value, json};
use shell_contract::IpcValue;
use shell_ipc_d1::sheets::engine::{EngineResult, WorkbookPrintAuthority, engine_error};
use shell_ipc_d1::value::{field, to_json};
use shell_ipc_m::hosts::{PrintProfile, PrintSurface, PrintWindowSpec};

use crate::host::HostContext;

use super::workbook_temp::WorkDir;

fn pass(surface: &mut dyn PrintSurface, request: &IpcValue, templates: Option<&IpcValue>, page_ranges: Option<String>) -> EngineResult<Vec<u8>> {
    let mut options = Map::new();
    for key in ["landscape", "pageSize", "margins", "scale"] {
        let value = field(request, key);
        if !matches!(value, IpcValue::Undefined) {
            let value = to_json(value).ok_or_else(|| engine_error(format!("invalid PDF option {key}")))?;
            options.insert(key.into(), jscompat::json_value_to_serde(&value));
        }
    }
    options.insert("preferCSSPageSize".into(), json!(true));
    options.insert("printBackground".into(), json!(true));
    if let Some(ranges) = page_ranges { options.insert("pageRanges".into(), json!(ranges)); }
    if let Some(templates) = templates {
        options.insert("displayHeaderFooter".into(), json!(true));
        for key in ["headerTemplate", "footerTemplate"] {
            let value = match field(templates, key) {
                IpcValue::String(value) => value.as_str(),
                _ => "<span></span>",
            };
            options.insert(key.into(), json!(value));
        }
    }
    surface.print_to_pdf(&Value::Object(options)).map_err(engine_error)
}

fn admit<R>(authority: &WorkbookPrintAuthority<'_>, action: impl FnOnce() -> R) -> Option<R> {
    match authority {
        WorkbookPrintAuthority::Interactive { origin, destination, cancel, pending } =>
            crate::ipc::native_owner::with_paired_effect(origin, destination, cancel, pending, action),
        WorkbookPrintAuthority::NativeInvocation { target, owner, cancel, pending } =>
            crate::ipc::native_owner::with_native_invocation_effect(target, owner, cancel, pending, action),
        WorkbookPrintAuthority::ExistingDocument { binding, participants, cancel, pending, additional } =>
            crate::ipc::native_owner::with_existing_document_print_effect(binding, participants, cancel, pending,
                pending, binding.operation().authority_generation(), &additional.iter().collect::<Vec<_>>(), action),
    }
}

fn host(name: &str, input: &Value, authority: Option<&WorkbookPrintAuthority<'_>>) -> EngineResult<Value> {
    match authority {
        Some(WorkbookPrintAuthority::Interactive { origin, destination, cancel, pending }) =>
            crate::engine_host::call_owned_pair(name, input, origin, destination, cancel, pending),
        Some(WorkbookPrintAuthority::NativeInvocation { target, owner, cancel, pending }) =>
            crate::engine_host::call_native_invocation(name, input, target, owner, cancel, pending),
        Some(WorkbookPrintAuthority::ExistingDocument { binding, participants, cancel, pending, additional }) =>
            crate::engine_host::call_existing_document_save_with_timeout(name, input, binding, participants, cancel, pending, additional, std::time::Duration::from_secs(120)),
        None => seu_host::call(name, input),
    }.map_err(|error| engine_error(format!("{}: {}", error.name, error.message)))
}

pub(super) fn render(cx: &HostContext, html: &str, request: &IpcValue) -> EngineResult<Vec<u8>> {
    render_with_owner(cx, html, request, None)
}

pub(super) fn render_owned(
    cx: &HostContext,
    html: &str,
    request: &IpcValue,
    authority: WorkbookPrintAuthority<'_>,
) -> EngineResult<Vec<u8>> {
    admit(&authority, || ())
        .ok_or_else(|| engine_error("The PDF export document is no longer available."))?;
    let bytes = render_with_owner(cx, html, request, Some(&authority))?;
    admit(&authority, || bytes)
        .ok_or_else(|| engine_error("The PDF export document is no longer available."))
}

fn render_with_owner(
    cx: &HostContext,
    html: &str,
    request: &IpcValue,
    owner: Option<&WorkbookPrintAuthority<'_>>,
) -> EngineResult<Vec<u8>> {
    let dir = WorkDir::create("ai-excel-pdf-")?;
    let page = dir.path().join("print.html");
    std::fs::write(&page, html).map_err(|error| engine_error(error.to_string()))?;
    let path = page.to_str().ok_or_else(|| engine_error("print page path is not UTF-8"))?;
    let spec = PrintWindowSpec {
        profile: PrintProfile::Static, width: None, height: None, parent: None, dialog_chrome: false,
    };
    let mut surface = match owner {
        Some(WorkbookPrintAuthority::Interactive { origin, destination, cancel, pending }) =>
            super::super::print::window_owned(cx, spec, origin, destination, cancel, pending).map_err(engine_error)?,
        Some(WorkbookPrintAuthority::NativeInvocation { target, owner, cancel, pending }) =>
            super::super::print::window_native_invocation(cx, spec, target, owner, cancel, pending).map_err(engine_error)?,
        Some(WorkbookPrintAuthority::ExistingDocument { binding, participants, cancel, pending, additional }) =>
            super::super::print::window_existing_document(cx, spec, binding, participants, cancel, pending, additional).map_err(engine_error)?,
        None => super::super::print::window(cx.app.clone(), cx.ui.clone(), cx.views.clone(), spec),
    };
    surface.load_file(path).map_err(engine_error)?;
    let first = field(request, "firstPage");
    let even = field(request, "evenPages");
    let flags = VariantFlags { has_first: !matches!(first, IpcValue::Undefined), has_even: !matches!(even, IpcValue::Undefined) };
    let empty = IpcValue::Object(Vec::new());
    let has_odd = !matches!(field(request, "headerTemplate"), IpcValue::Undefined) || !matches!(field(request, "footerTemplate"), IpcValue::Undefined);
    let templates = if has_odd { Some(request) } else if flags.has_first || flags.has_even { Some(&empty) } else { None };
    let odd = pass(surface.as_mut(), request, templates, None)?;
    if !flags.has_first && !flags.has_even { return Ok(odd); }
    let encoded_odd = STANDARD.encode(&odd);
    let count = host("pdf.pageCount", &json!({ "bytesBase64": encoded_odd }), owner)?.as_u64()
        .filter(|count| *count <= u32::MAX as u64).ok_or_else(|| engine_error("PDF engine returned invalid page count"))?;
    let first_pdf = if flags.has_first && count > 0 { Some(pass(surface.as_mut(), request, Some(first), Some("1".into()))?) } else { None };
    let ranges = even_page_ranges(count as f64);
    let even_pdf = if flags.has_even && !ranges.is_empty() { Some(pass(surface.as_mut(), request, Some(even), Some(ranges))?) } else { None };
    let steps: Vec<_> = stitch_plan(count as f64, flags).into_iter().map(|step| json!({ "source": step.source.as_str(), "index": step.index, "page": step.page })).collect();
    let output = host("pdf.stitchPages", &json!({
        "oddBase64": encoded_odd,
        "firstBase64": first_pdf.as_ref().map(|pdf| STANDARD.encode(pdf)),
        "evenBase64": even_pdf.as_ref().map(|pdf| STANDARD.encode(pdf)),
        "steps": steps,
    }), owner)?;
    let encoded = output.as_str().ok_or_else(|| engine_error("PDF engine returned invalid stitched PDF"))?;
    STANDARD.decode(encoded).map_err(|error| engine_error(format!("PDF engine returned invalid base64: {error}")))
}
