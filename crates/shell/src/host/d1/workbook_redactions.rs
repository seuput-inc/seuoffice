//! Workbook redaction part and defined-name mirror from workbook-redaction-entries.ts.

use serde_json::{Value, json};
use shell_contract::IpcValue;
use shell_ipc_d1::sheets::engine::EngineResult;
use shell_ipc_d1::value::from_json;

use super::workbook_rpc::Rpc;

const PART: &str = "xl/gxRedactions.json";
const WORKBOOK: &str = "xl/workbook.xml";

fn mirror(rpc: &Rpc, snapshot: &str) -> Value {
    let result = rpc
        .read_text(snapshot, WORKBOOK)
        .map_err(|error| error.message)
        .and_then(|text| file_parse::xlsx::workbook_redaction_mirror(&text));
    match result {
        Ok(Value::Array(states)) if states.is_empty() => json!({ "status": "absent" }),
        Ok(states) => json!({ "status": "ok", "states": states, "source": "mirror" }),
        Err(error) => json!({ "status": "unreadable", "error": error }),
    }
}

pub(super) fn read(rpc: &Rpc, snapshot: &str) -> EngineResult<IpcValue> {
    let names = rpc.names(snapshot)?;
    let result = if names.iter().any(|name| name == PART) {
        let part = rpc
            .read_text(snapshot, PART)
            .map_err(|error| error.message)
            .and_then(|text| file_parse::xlsx::workbook_redaction_part(&text));
        match part {
            Ok(states) => json!({ "status": "ok", "states": states, "source": "part" }),
            Err(error) => {
                let mirrored = mirror(rpc, snapshot);
                if mirrored.get("status").and_then(Value::as_str) == Some("ok") {
                    mirrored
                } else {
                    json!({ "status": "unreadable", "error": error })
                }
            }
        }
    } else {
        mirror(rpc, snapshot)
    };
    super::workbook_schema::parse(
        "redactions",
        &from_json(&jscompat::json_value_from_serde(&result)),
    )
}
