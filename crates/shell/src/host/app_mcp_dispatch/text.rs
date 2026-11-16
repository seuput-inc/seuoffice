use std::rc::Rc;

use jscompat::JsonValue;
use main_core::shell::mcp::mcp_server::{HostCall, Settled};
use shell_contract::editors::DocumentEditors;
use shell_contract::ids::ViewId;
use shell_contract::runtime::Services;

use super::visible::{string, view_id};

pub(super) fn dispatch(
    services: &Services,
    editors: &[Rc<dyn DocumentEditors>],
    markdown: Option<Rc<shell_ipc_d2::markdown::MarkdownEditors>>,
    html: Option<Rc<shell_ipc_d2::html::HtmlEditors>>,
    call: HostCall,
    done: Box<dyn FnOnce(Settled)>,
) {
    let id = match view_id(call.args.first()).and_then(|id| {
        u32::try_from(id)
            .map(ViewId)
            .map_err(|_| "invalid document view id".into())
    }) {
        Ok(id) => id,
        Err(error) => return done(Err(error)),
    };
    let Some(sender) = services.views.sender_of(id) else {
        return done(Err("the document view is closed".into()));
    };
    let expected = match call.control {
        "MarkdownBridge" => shell_contract::Surface::Markdown,
        "HtmlBridge" => shell_contract::Surface::Html,
        "MindmapBridge" => shell_contract::Surface::Mindmap,
        "HwpBridge" => shell_contract::Surface::Hwp,
        "EpubBridge" => shell_contract::Surface::Epub,
        "DiagramBridge" => shell_contract::Surface::Diagram,
        _ => return done(Err(format!("unknown editor bridge {}", call.control))),
    };
    if sender.surface != expected {
        return done(Err("the document view belongs to another editor".into()));
    }
    if call.method == "discard" {
        match call.control {
            "MarkdownBridge" => match markdown {
                Some(editor) if editor.owns(id) => editor.discard_pending_assets(id),
                _ => return done(Err("the Markdown editor is unavailable".into())),
            },
            "HtmlBridge" => match html {
                Some(editor) if editor.owns(id) => editor.discard_pending_assets(id),
                _ => return done(Err("the HTML editor is unavailable".into())),
            },
            _ => return done(Err(format!("unknown {}.{}", call.control, call.method))),
        }
        return done(Ok(None));
    }
    let Some(editor) = editors.iter().find(|editor| editor.owns(id)).cloned() else {
        return done(Err("the document editor is unavailable".into()));
    };
    match call.method {
        "read" => {
            let views = services.views.clone();
            editor.read_text(
                id,
                Box::new(move |result| {
                    if views.sender_of(id).is_none() {
                        return done(Err("the document view is closed".into()));
                    }
                    done(
                        result
                            .map(|text| Some(JsonValue::String(text)))
                            .map_err(|error| error.to_string()),
                    );
                }),
            );
        }
        "save" => {
            let path: std::path::PathBuf = match string(call.args.get(1)) {
                Ok(path) => path.into(),
                Err(error) => return done(Err(error)),
            };
            let Some(generation) = services.views.renderer_generation(id) else {
                return done(Err("the document renderer is unavailable".into()));
            };
            let Some(effects) = services
                .hosts
                .get::<shell_contract::hosts::OwnedEffectsHost>()
            else {
                return done(Err("native document effects are unavailable".into()));
            };
            let owner = match effects
                .capture_owner(id, &generation)
                .map_err(|error| error.to_string())
                .and_then(|owner| {
                    editor
                        .capture_document_owner(id, owner)
                        .map_err(|error| error.to_string())
                }) {
                Ok(owner) => owner,
                Err(error) => return done(Err(error)),
            };
            let cancel = shell_contract::runtime::CancelToken::default();
            let pending = shell_contract::runtime::CancelToken::default();
            let binding = match crate::host::native_commands::issue_existing_save(
                expected,
                id,
                shell_contract::ipc::NativeSaveSelection::ExactPath(path.clone()),
                owner,
                cancel.clone(),
            ) {
                Ok(binding) => binding,
                Err(error) => return done(Err(error)),
            };
            editor.save_to_path_native(
                id,
                path,
                binding,
                cancel,
                pending,
                Box::new(move |result| {
                    done(result.map(|()| None).map_err(|error| error.to_string()));
                }),
            );
        }
        _ => done(Err(format!("unknown {}.{}", call.control, call.method))),
    }
}
