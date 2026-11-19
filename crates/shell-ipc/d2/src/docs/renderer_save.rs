//! Save a Docs view through the existing MCP renderer bridge.

use std::path::PathBuf;
use std::rc::Rc;

use jscompat::{JsObject, JsonValue};
use main_core::shell::mcp::tools::document_tools::McpEditorCommandName;
use shell_contract::editors::Then;
use shell_contract::{Surface, ViewId};
use shell_ipc_a::mcp::McpBridgeControl;

use super::Docs;
use crate::kit;

pub(super) fn save_to_path(docs: &Rc<Docs>, view: ViewId, path: PathBuf, then: Then<()>) {
    if docs.is_torn_down(view)
        || !docs
            .services
            .views
            .sender_of(view)
            .is_some_and(|sender| sender.surface == Surface::Docs)
    {
        return then(Err(kit::error("the target document is no longer open")));
    }
    let Some(target) = path
        .to_str()
        .filter(|_| path.is_absolute())
        .map(str::to_owned)
    else {
        return then(Err(kit::error("save_document requires an absolute path")));
    };
    let Some(bridge) = docs
        .provided
        .upgrade()
        .and_then(|provided| provided.all::<dyn McpBridgeControl>().into_iter().next())
    else {
        return then(Err(kit::error(
            "the Word editor's MCP bridge is unavailable",
        )));
    };
    let mut payload = JsObject::new();
    payload.set("path", JsonValue::String(target.clone()));
    payload.set("overwrite", JsonValue::Bool(true));
    let docs = docs.clone();
    bridge.docs_run_command(
        i64::from(view.0),
        McpEditorCommandName::SaveDocument,
        Some(JsonValue::Object(payload)),
        Box::new(move |outcome| {
            let result = match outcome {
                Err(error) => return then(Err(error)),
                Ok(Some(JsonValue::Object(result))) => result,
                _ => return then(Err(kit::error("the document save returned no result"))),
            };
            if result.get("ok") != Some(&JsonValue::Bool(true))
                || result.get("path") != Some(&JsonValue::String(target))
            {
                return then(Err(kit::error(
                    "the document save returned an invalid result",
                )));
            }
            let services = docs.services.clone();
            kit::on_worker(
                &services,
                move || {
                    std::fs::metadata(&path)
                        .map_err(|error| {
                            kit::error(format!("saved document is unavailable: {error}"))
                        })
                        .and_then(|metadata| {
                            if metadata.is_file() {
                                Ok(())
                            } else {
                                Err(kit::error("saved document is not a file"))
                            }
                        })
                },
                move |result| {
                    if docs.is_torn_down(view)
                        || !docs
                            .services
                            .views
                            .sender_of(view)
                            .is_some_and(|sender| sender.surface == Surface::Docs)
                    {
                        then(Err(kit::error("the target document is no longer open")));
                    } else {
                        then(result);
                    }
                },
            );
        }),
    );
}
