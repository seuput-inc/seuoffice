use jscompat::JsonValue;
use std::rc::Rc;
use main_core::shell::mcp::mcp_server::{HostCall, Settled};
use main_core::shell::mcp::tools::document_tools::McpEditorCommandName;
use main_core::shell::mcp::tools::sheets_tools::SheetsCommandName;
use shell_ipc_a::mcp::McpBridgeControl;

pub(super) fn dispatch(bridge: Rc<dyn McpBridgeControl>, call: HostCall, done: Box<dyn FnOnce(Settled)>) {
    let callback = Box::new(move |answer: Result<Option<JsonValue>, shell_contract::IpcError>| {
        done(answer.map_err(|error| error.to_string()));
    });
    match (call.control, call.method) {
        ("DocsControl", "openBlankTab") => bridge.docs_open_blank_tab(callback),
        ("SheetsControl", "openBlankTab") => bridge.sheets_open_blank_tab(callback),
        ("DocsControl", "runCommand") => {
            let parsed = (|| {
                let view = view_id(call.args.first())?;
                let command = match string(call.args.get(1))? {
                    "insert_content" => McpEditorCommandName::InsertContent,
                    "replace_blocks" => McpEditorCommandName::ReplaceBlocks,
                    "apply_ops" => McpEditorCommandName::ApplyOps,
                    "insert_picture" => McpEditorCommandName::InsertPicture,
                    "read_document" => McpEditorCommandName::ReadDocument,
                    "save_document" => McpEditorCommandName::SaveDocument,
                    _ => return Err("unknown Docs MCP command".to_owned()),
                };
                Ok((view, command))
            })();
            match parsed {
                Ok((view, command)) => bridge.docs_run_command(view, command, call.args.get(2).cloned(), callback),
                Err(error) => callback(Err(shell_contract::IpcError { name: None, message: error })),
            }
        }
        ("SheetsControl", "runCommand") => {
            let parsed = (|| {
                let view = view_id(call.args.first())?;
                let command = match string(call.args.get(1))? {
                    "apply_ops" => SheetsCommandName::ApplyOps,
                    "read_sheet" => SheetsCommandName::ReadSheet,
                    "save_sheet" => SheetsCommandName::SaveSheet,
                    _ => return Err("unknown Sheets MCP command".to_owned()),
                };
                Ok((view, command))
            })();
            match parsed {
                Ok((view, command)) => bridge.sheets_run_command(view, command, call.args.get(2).cloned(), callback),
                Err(error) => callback(Err(shell_contract::IpcError { name: None, message: error })),
            }
        }
        _ => callback(Err(shell_contract::IpcError { name: None, message: format!("unknown {}.{}", call.control, call.method) })),
    }
}

pub(super) fn view_id(value: Option<&JsonValue>) -> Result<i64, String> {
    match value {
        Some(JsonValue::Number(number)) if number.is_finite() && number.fract() == 0.0 && *number >= 1.0 && *number <= u32::MAX as f64 => Ok(*number as i64),
        _ => Err("invalid document view id".to_owned()),
    }
}

pub(super) fn string(value: Option<&JsonValue>) -> Result<&str, String> {
    match value {
        Some(JsonValue::String(value)) => Ok(value),
        _ => Err("expected string argument".to_owned()),
    }
}
