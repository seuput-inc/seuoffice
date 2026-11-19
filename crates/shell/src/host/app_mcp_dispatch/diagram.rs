use jscompat::{JsObject, JsonValue};
use main_core::shell::mcp::mcp_server::{HostCall, Settled};
use shell_contract::runtime::Services;
use shell_contract::{IpcValue, Surface, ViewId};

use super::visible::view_id;

pub(super) fn dispatch(services: &Services, call: HostCall, done: Box<dyn FnOnce(Settled)>) {
    let id = match view_id(call.args.first()).and_then(|id| {
        u32::try_from(id)
            .map(ViewId)
            .map_err(|_| "invalid document view id".to_owned())
    }) {
        Ok(id) => id,
        Err(error) => return done(Err(error)),
    };
    if !services
        .views
        .sender_of(id)
        .is_some_and(|sender| sender.surface == Surface::Diagram)
    {
        return done(Err(
            "the diagram view is closed or belongs to another editor".to_owned(),
        ));
    }
    let Some(diagram) = shell_ipc_d1::diagram::views() else {
        return done(Err("the diagram editor is unavailable".to_owned()));
    };
    match call.method {
        "readPage" => {
            let page = match call.args.get(1) {
                None | Some(JsonValue::Null) => None,
                Some(JsonValue::String(page)) => Some(page.as_str()),
                _ => return done(Err("invalid diagram page".to_owned())),
            };
            diagram.read_page(
                id,
                page,
                Box::new(move |result| {
                    done(result.and_then(|read| {
                        let mut result = JsObject::new();
                        result.set("text", JsonValue::String(read.text));
                        if let Some(pages) = read.pages {
                            result.set("pages", to_json(&pages)?);
                        }
                        if let Some(page) = read.page {
                            result.set("page", to_json(&page)?);
                        }
                        Ok(Some(JsonValue::Object(result)))
                    }))
                }),
            );
        }
        "applyOps" => {
            let Some(JsonValue::Array(ops)) = call.args.get(1) else {
                return done(Err("invalid diagram operations".to_owned()));
            };
            diagram.apply_ops(
                id,
                ops.iter().map(from_json).collect(),
                Box::new(move |result| {
                    done(result.and_then(|value| to_json(&value).map(Some)));
                }),
            );
        }
        _ => done(Err(format!("unknown diagram query {}", call.method))),
    }
}

fn from_json(value: &JsonValue) -> IpcValue {
    match value {
        JsonValue::Null => IpcValue::Null,
        JsonValue::Bool(v) => IpcValue::Bool(*v),
        JsonValue::Number(v) => IpcValue::Number(*v),
        JsonValue::String(v) => IpcValue::String(v.clone()),
        JsonValue::Array(values) => IpcValue::Array(values.iter().map(from_json).collect()),
        JsonValue::Object(values) => IpcValue::Object(
            values
                .entries()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), from_json(value)))
                .collect(),
        ),
    }
}

fn to_json(value: &IpcValue) -> Result<JsonValue, String> {
    Ok(match value {
        IpcValue::Null => JsonValue::Null,
        IpcValue::Bool(v) => JsonValue::Bool(*v),
        IpcValue::Number(v) => JsonValue::Number(*v),
        IpcValue::String(v) => JsonValue::String(v.clone()),
        IpcValue::Array(values) => {
            JsonValue::Array(values.iter().map(to_json).collect::<Result<_, _>>()?)
        }
        IpcValue::Object(values) => JsonValue::Object(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), to_json(value)?)))
                .collect::<Result<JsObject<_>, String>>()?,
        ),
        _ => return Err("diagram returned a non-JSON value".to_owned()),
    })
}
