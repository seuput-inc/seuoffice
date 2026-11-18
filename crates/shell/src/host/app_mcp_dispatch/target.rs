use std::rc::Rc;

use jscompat::JsonValue;
use main_core::shell::mcp::mcp_server::{HostCall, Settled};
use main_core::shell::mcp::tools::formats::SessionFamily;
use main_core::shell::mcp::tools::open_documents_tools::resolve_open_document_of_family;
use main_core::shell::tab_manager::TabLoadResult;
use shell_contract::runtime::Services;
use shell_ipc_w::tabs::TabsState;

use super::visible::string;

pub(super) fn dispatch(state: Rc<TabsState>, services: Services, call: HostCall, done: Box<dyn FnOnce(Settled)>) {
    if call.method != "resolve" {
        return done(Err(format!("unknown TargetResolver.{}", call.method)));
    }
    let target = match string(call.args.first()) {
        Ok(target) => target.to_owned(),
        Err(error) => return done(Err(error)),
    };
    let family = match string(call.args.get(1)).ok().and_then(SessionFamily::parse) {
        Some(family) => family.editor(),
        None => return done(Err("invalid document family".to_owned())),
    };
    let focus = matches!(call.args.get(2).and_then(|v| v.get("focus")).as_deref(), Some(JsonValue::Bool(true)));
    let next = state.clone();
    let lookup = state.clone();
    state.open_documents(Box::new(move |result| {
        let documents = match result {
            Ok(documents) => documents,
            Err(error) => return done(Err(error)),
        };
        let tab = match resolve_open_document_of_family(&documents, &target, family) {
            Ok(tab) => tab.clone(),
            Err(error) => return done(Err(error)),
        };
        let id = tab.id.clone();
        let finish = move |answer: Result<TabLoadResult<shell_contract::ViewId>, String>| {
            match answer {
                Ok(TabLoadResult::Ready(view)) if services.views.sender_of(view).is_some() => {
                    if focus && !tab.active && !next.reveal_tab(&tab.id) {
                        eprintln!("[mcp] could not bring the target document into view: {}", tab.id);
                    }
                    done(Ok(Some(JsonValue::Number(f64::from(view.0)))));
                }
                Ok(TabLoadResult::NeedsUser) => done(Err(format!("\"{}\" needs user interaction before it can open", tab.title))),
                Ok(_) => done(Err(format!("\"{}\" is no longer open", tab.title))),
                Err(error) => done(Err(error)),
            }
        };
        if lookup.is_detached_tab(&id) {
            match lookup.tab_contents(&id) {
                Some(view) => finish(Ok(TabLoadResult::Ready(view))),
                None => finish(Ok(TabLoadResult::Missing)),
            }
        } else {
            lookup.ensure_tab_loaded(&id, Box::new(finish));
        }
    }));
}
