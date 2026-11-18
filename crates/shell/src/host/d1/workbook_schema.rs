//! Lossless workbook schema validation through the original Zod schemas.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use shell_contract::{IpcError, IpcValue};
use shell_ipc_d1::sheets::engine::{EngineResult, engine_error};

pub(super) fn parse(schema: &str, value: &IpcValue) -> EngineResult<IpcValue> {
    let (header, buffers) = shell_core::wire::encode(value);
    let buffers: Vec<String> = buffers.iter().map(|bytes| STANDARD.encode(bytes)).collect();
    let input = serde_json::json!({ "schema": schema, "header": header, "buffers": buffers });
    let output = seu_host::call("sheets.schema", &input).map_err(|error| IpcError {
        name: if error.name == "ZodError" { None } else { Some(error.name) },
        message: error.message,
    })?;
    let header = output.get("header").ok_or_else(|| engine_error("workbook schema returned no header"))?;
    let buffers = output.get("buffers").and_then(|v| v.as_array())
        .ok_or_else(|| engine_error("workbook schema returned invalid buffers"))?
        .iter().map(|value| {
            let text = value.as_str().ok_or_else(|| engine_error("workbook schema returned invalid buffer"))?;
            STANDARD.decode(text).map_err(|error| engine_error(format!("workbook schema returned invalid base64: {error}")))
        }).collect::<EngineResult<Vec<_>>>()?;
    shell_core::wire::decode(header, &buffers).map_err(|error| engine_error(error.to_string()))
}
