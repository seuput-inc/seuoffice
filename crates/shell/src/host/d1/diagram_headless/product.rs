use main_core::packages::electron_utils::atomic_write::atomic_write_file_authorized;
use main_core::packages::electron_utils::headless_export::HeadlessExportRequest;
use main_core::shell::headless_export::HeadlessExportProduct;
use shell_contract::IpcValue;
use shell_ipc_d1::diagram::{DiagramFile, OpenFailure};
use shell_ipc_d1::value::{as_bytes, as_num, as_str, field, from_json, to_json};

pub(super) fn job(format: &str, request: &HeadlessExportRequest) -> Result<IpcValue, String> {
    let mut fields = vec![
        (
            "kind".into(),
            IpcValue::String(
                if matches!(format, "pdf" | "png" | "svg") {
                    "export"
                } else {
                    "write"
                }
                .into(),
            ),
        ),
        ("format".into(), IpcValue::String(format.to_owned())),
    ];
    if !matches!(format, "pdf" | "png" | "svg") {
        let ops = if let Some(path) = &request.ops {
            let bytes = std::fs::read(path)
                .map_err(|error| format!("--ops {path} is not a JSON file: {error}"))?;
            let parsed: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("--ops {path} is not a JSON file: {error}"))?;
            let list = if parsed.is_array() {
                &parsed
            } else {
                parsed.get("ops").unwrap_or(&serde_json::Value::Null)
            };
            if !list.is_array() {
                return Err(format!("--ops {path} must hold a JSON array of ops"));
            }
            from_json(&jscompat::json_value_from_serde(list))
        } else {
            IpcValue::Array(Vec::new())
        };
        fields.push(("ops".into(), ops));
        fields.push((
            "mode".into(),
            IpcValue::String(request.batch.unwrap_or("all-or-nothing").into()),
        ));
    }
    if let Some(page) = request.page {
        fields.push(("page".into(), IpcValue::Number(page)));
    }
    Ok(IpcValue::Object(fields))
}

pub(super) fn product(
    input: &str,
    out: &str,
    format: &str,
    page: Option<f64>,
    report: &IpcValue,
    open: &mut dyn FnMut(&str, &[u8]) -> Result<Result<DiagramFile, OpenFailure>, String>,
    render_pdf: &mut dyn FnMut(
        &[main_core::diagram::print_document::PrintPage],
        &str,
    ) -> Result<Vec<u8>, String>,
    admit: &mut dyn FnMut(&mut dyn FnMut()) -> Result<(), main_core::node_fs::NodeError>,
) -> Result<HeadlessExportProduct, String> {
    if !matches!(field(report, "ok"), IpcValue::Bool(true)) {
        return Err(as_str(field(report, "error"))
            .unwrap_or("the diagram page sent no report")
            .to_owned());
    }
    let wrong = || "the diagram page answered another job".to_owned();
    let mut detail = serde_json::Map::new();
    let paths = match format {
        "pdf" => {
            if as_str(field(report, "kind")) != Some("pdf") {
                return Err(wrong());
            }
            let pages =
                shell_ipc_d1::diagram::print_pages_of(field(report, "pages")).ok_or_else(wrong)?;
            let bytes = render_pdf(&pages, &shell_ipc_d1::diagram::diagram_base_name(input))?;
            atomic_write_file_authorized(out, &bytes, admit).map_err(|error| error.message)?;
            vec![out.to_owned()]
        }
        "png" | "svg" => {
            if as_str(field(report, "kind")) != Some("images")
                || as_str(field(report, "format")) != Some(format)
            {
                return Err(wrong());
            }
            let IpcValue::Array(pages) = field(report, "pages") else {
                return Err(wrong());
            };
            if pages.is_empty() {
                return Err(wrong());
            }
            let mut files = Vec::with_capacity(pages.len());
            for item in pages {
                let number = as_num(field(item, "number")).ok_or_else(wrong)?;
                let bytes = as_bytes(field(item, "bytes"))
                    .filter(|bytes| shell_ipc_d1::diagram::export_looks_like(format, bytes))
                    .ok_or_else(|| {
                        format!(
                            "page {} is not a {format} picture",
                            jscompat::number_to_string(number)
                        )
                    })?;
                files.push((number, bytes));
            }
            let numbers: Vec<_> = files.iter().map(|(number, _)| *number).collect();
            let paths = main_core::diagram::export_paths::page_file_paths(
                out,
                format,
                &numbers,
                page.is_none(),
            );
            for (path, (_, bytes)) in paths.iter().zip(files) {
                atomic_write_file_authorized(path, bytes, admit).map_err(|error| error.message)?;
            }
            paths
        }
        _ => {
            if as_str(field(report, "kind")) != Some("write") {
                return Err(wrong());
            }
            let batch = to_json(field(report, "batch")).ok_or_else(wrong)?;
            detail.insert("batch".into(), jscompat::json_value_to_serde(&batch));
            detail.insert("format".into(), format.into());
            match as_bytes(field(report, "bytes")) {
                None => Vec::new(),
                Some(bytes) => {
                    let name = jscompat::node_path::host::basename(out, None);
                    if let Err(failure) = open(&name, bytes)? {
                        return Err(format!(
                            "refusing to write a diagram that would not open ({})",
                            failure.reason
                        ));
                    }
                    atomic_write_file_authorized(out, bytes, admit)
                        .map_err(|error| error.message)?;
                    vec![out.to_owned()]
                }
            }
        }
    };
    detail.insert("requests".into(), super::network::requests()?);
    Ok(HeadlessExportProduct {
        paths,
        detail: Some(serde_json::Value::Object(detail)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_ipc_d1::value::field;

    #[test]
    fn write_job_carries_page_and_batch() {
        let request = HeadlessExportRequest {
            input: "input.drawio".into(),
            target_format: "drawio",
            out_path: "out.drawio".into(),
            json: true,
            page: Some(2.0),
            ops: None,
            batch: Some("best-effort"),
        };
        let job = job("drawio", &request).expect("job");
        assert_eq!(field(&job, "kind"), &IpcValue::String("write".into()));
        assert_eq!(field(&job, "page"), &IpcValue::Number(2.0));
        assert_eq!(field(&job, "mode"), &IpcValue::String("best-effort".into()));
        assert_eq!(field(&job, "ops"), &IpcValue::Array(Vec::new()));
    }
}
