mod diagram;
mod recovery;
mod target;
mod text;
mod visible;

use std::rc::Rc;

use jscompat::JsonValue;
use main_core::shell::mcp::diagram_bridge::{create_diagram_control, DiagramBridgeSync};
use main_core::shell::mcp::mcp_server::{HostCall, Settled, ToolStep};
use main_core::shell::mcp::open_documents_bridge::{create_open_documents_control, Dep, OpenBridgeSync};
use main_core::shell::mcp::tools::diagram_tools::tab_of_json;
use main_core::shell::mcp::tools::open_documents_tools::{tab_json, Unsaved};
use main_core::shell::tab_manager::TabLoadResult;
use shell_contract::editors::DocumentEditors;
use shell_contract::provided::Provided;
use shell_contract::runtime::Services;
use shell_ipc_a::mcp::McpBridgeControl;
use shell_ipc_w::tabs::TabsState;

struct Context {
    services: Services,
    tabs: Rc<TabsState>,
    bridge: Option<Rc<dyn McpBridgeControl>>,
    editors: Vec<Rc<dyn DocumentEditors>>,
    markdown: Option<Rc<shell_ipc_d2::markdown::MarkdownEditors>>,
    html: Option<Rc<shell_ipc_d2::html::HtmlEditors>>,
    hwp_available: bool,
    epub_available: bool,
    recovery: recovery::Recovery,
}

impl OpenBridgeSync for Context {
    fn wired(&self, dep: Dep) -> bool {
        match dep {
            Dep::Docs | Dep::Sheets => self.bridge.is_some(),
            Dep::Slides | Dep::SlidesDiscard => shell_ipc_d1::slides::views().is_some(),
            Dep::Markdown => self.markdown.is_some(),
            Dep::Html => self.html.is_some(),
            Dep::Mindmap => shell_ipc_d1::mindmap::handle().is_some(),
            Dep::Diagram => shell_ipc_d1::diagram::views().is_some(),
            Dep::Hwp => self.hwp_available,
            Dep::Epub => self.epub_available,
            Dep::RecoveryDiscard => true,
        }
    }
    fn is_destroyed(&self, wc_id: i64) -> bool {
        u32::try_from(wc_id).ok().and_then(|id| self.services.views.sender_of(shell_contract::ViewId(id))).is_none()
    }
    fn close_tab(&self, tab_id: &str) -> bool {
        if self.tabs.is_detached_tab(tab_id) {
            return self.tabs.tab_contents(tab_id).is_some_and(|view| self.tabs.close_document_without_prompt(view));
        }
        self.tabs.with_manager(|manager, host| manager.close_tab_without_prompt(host, tab_id)).unwrap_or(false)
    }
    fn is_unloaded(&self, tab_id: &str) -> Option<bool> {
        self.tabs.with_manager(|manager, _| manager.is_tab_unloaded(tab_id))
    }
    fn default_save_dir(&self) -> String { super::w_tabs::default_save_dir(&self.services) }
    fn activate(&self, tab_id: &str) -> Result<(), String> {
        if self.tabs.reveal_tab(tab_id) { Ok(()) } else { Err("the document is no longer open".into()) }
    }
    fn reveal_window(&self, _tab_id: &str) -> Result<(), String> { Ok(()) }
    fn warn(&self, message: &str, error: &str) { eprintln!("{message} {error}"); }
}

impl DiagramBridgeSync for Context {
    fn is_destroyed(&self, wc_id: i64) -> bool { OpenBridgeSync::is_destroyed(self, wc_id) }
    fn is_detached(&self, tab_id: &str) -> Option<bool> { Some(self.tabs.is_detached_tab(tab_id)) }
    fn activate(&self, tab_id: &str) -> Result<(), String> { OpenBridgeSync::activate(self, tab_id) }
    fn reveal_window(&self, tab_id: &str) -> Result<(), String> { OpenBridgeSync::reveal_window(self, tab_id) }
    fn warn(&self, message: &str, error: &str) { OpenBridgeSync::warn(self, message, error); }
}

pub(crate) fn dispatch(provided: &Provided, services: &Services, call: HostCall, done: Box<dyn FnOnce(Settled) + Send>) {
    let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
        return done(Err("the tab manager is unavailable".into()));
    };
    let context = Rc::new(Context {
        services: services.clone(),
        tabs,
        bridge: provided.all::<dyn McpBridgeControl>().into_iter().next(),
        editors: provided.all::<dyn DocumentEditors>(),
        markdown: provided.all::<shell_ipc_d2::markdown::MarkdownEditors>().into_iter().next(),
        html: provided.all::<shell_ipc_d2::html::HtmlEditors>().into_iter().next(),
        hwp_available: !provided.all::<shell_ipc_d2::hwp::HwpEditors>().is_empty(),
        epub_available: !provided.all::<shell_ipc_d2::epub::EpubEditors>().is_empty(),
        recovery: recovery::Recovery::new(provided),
    });
    run_call(context, call, Box::new(done));
}

fn run_step(context: Rc<Context>, step: ToolStep<'static>, done: Box<dyn FnOnce(Settled)>) {
    match step {
        ToolStep::Done(result) => done(result),
        ToolStep::Await { call, then } => {
            let next = context.clone();
            run_call(context, call, Box::new(move |result| run_step(next, then(result), done)));
        }
    }
}

fn run_call(context: Rc<Context>, call: HostCall, done: Box<dyn FnOnce(Settled)>) {
    match call.control {
        "NativeOpen" if call.method == "openPath" => {
            use main_core::shell::app_routing::{app_for_path, DocumentTabKind};
            use shell_contract::{Surface, ipc::NativeEditorCommand, runtime::CancelToken};
            let path = match visible::string(call.args.first()) {
                Ok(path) => path.to_owned(),
                Err(error) => return done(Err(error)),
            };
            let surface = match app_for_path(&path) {
                Some(DocumentTabKind::Docs) => Surface::Docs,
                Some(DocumentTabKind::Sheets) => Surface::Sheets,
                Some(DocumentTabKind::Slides) => Surface::Slides,
                Some(DocumentTabKind::Pdf) => Surface::Pdf,
                Some(DocumentTabKind::Markdown) => Surface::Markdown,
                Some(DocumentTabKind::Html) => Surface::Html,
                Some(DocumentTabKind::Mindmap) => Surface::Mindmap,
                Some(DocumentTabKind::Hwp) => Surface::Hwp,
                Some(DocumentTabKind::Epub) => Surface::Epub,
                Some(DocumentTabKind::Diagram) => Surface::Diagram,
                None => Surface::Shell,
            };
            let cancel = CancelToken::default();
            let operation = match super::native_commands::issue(
                NativeEditorCommand::OpenFile { surface, path: std::path::PathBuf::from(&path) },
                cancel.clone(),
            ) {
                Ok(operation) => operation,
                Err(error) => return done(Err(error)),
            };
            context.tabs.route_document_path_native(path, operation, Box::new(move |result| {
                drop(cancel);
                done(result.map(|opened| Some(JsonValue::Bool(opened))));
            }));
        }
        "DocsControl" | "SheetsControl" => match &context.bridge {
            Some(bridge) => visible::dispatch(bridge.clone(), call, done),
            None => done(Err("visible editor bridge is unavailable".into())),
        },
        "TargetResolver" => target::dispatch(context.tabs.clone(), context.services.clone(), call, done),
        "MarkdownBridge" | "HtmlBridge" | "MindmapBridge" | "HwpBridge" | "EpubBridge" | "DiagramBridge" => {
            text::dispatch(&context.services, &context.editors, context.markdown.clone(), context.html.clone(), call, done)
        }
        "DiagramBridgeDeps" if matches!(call.method, "readPage" | "applyOps") => diagram::dispatch(&context.services, call, done),
        "OpenDocumentsBridgeDeps" if matches!(call.method, "slidesDiscard" | "recoveryDiscard") => done(context.recovery.dispatch(&context.services, call)),
        "OpenDocumentsBridgeDeps" | "DiagramBridgeDeps" if call.method == "list" => {
            context.tabs.open_documents(Box::new(move |result| done(result.map(|tabs| Some(JsonValue::Array(tabs.iter().map(tab_json).collect()))))));
        }
        "OpenDocumentsBridgeDeps" | "DiagramBridgeDeps" if call.method == "webContentsFor" => {
            let id = match visible::string(call.args.first()) { Ok(id) => id.to_owned(), Err(error) => return done(Err(error)) };
            if context.tabs.is_detached_tab(&id) {
                return done(Ok(context.tabs.tab_contents(&id).map(|view| JsonValue::Number(f64::from(view.0)))));
            }
            context.tabs.ensure_tab_loaded(&id, Box::new(move |result| done(match result {
                Ok(TabLoadResult::Ready(view)) => Ok(Some(JsonValue::Number(f64::from(view.0)))),
                Ok(TabLoadResult::Missing) => Ok(None),
                Ok(TabLoadResult::NeedsUser) => Err("the document needs user interaction before it can open".into()),
                Err(error) => Err(error),
            })));
        }
        "OpenDocumentsControl" => {
            let control = create_open_documents_control(context.clone());
            let step = match call.method {
                "list" => control.list(),
                "read" | "save" | "close" => {
                    let Some(tab) = call.args.first() else { return done(Err("missing document".into())); };
                    let tab = tab_of_json(tab);
                    match call.method {
                        "read" => control.read(&tab),
                        "save" => control.save(&tab),
                        _ => {
                            let unsaved = match call.args.get(1).and_then(|v| v.get("unsaved")).as_deref() {
                                Some(JsonValue::String(value)) if value == "save" => Unsaved::Save,
                                Some(JsonValue::String(value)) if value == "discard" => Unsaved::Discard,
                                _ => return done(Err("invalid unsaved action".into())),
                            };
                            control.close(&tab, unsaved)
                        }
                    }
                }
                _ => return done(Err(format!("unknown OpenDocumentsControl.{}", call.method))),
            };
            run_step(context, step, done);
        }
        "DiagramControl" => {
            let control = create_diagram_control(context.clone());
            let step = match call.method {
                "list" => control.list(),
                "read" | "apply" => {
                    let Some(tab) = call.args.first() else { return done(Err("missing document".into())); };
                    let tab = tab_of_json(tab);
                    if call.method == "read" {
                        let page = match call.args.get(1) { None | Some(JsonValue::Null) => None, Some(JsonValue::String(page)) => Some(page.as_str()), _ => return done(Err("invalid diagram page".into())) };
                        control.read(&tab, page)
                    } else {
                        let Some(JsonValue::Array(ops)) = call.args.get(1) else { return done(Err("invalid diagram operations".into())); };
                        control.apply(&tab, ops.clone())
                    }
                }
                _ => return done(Err(format!("unknown DiagramControl.{}", call.method))),
            };
            run_step(context, step, done);
        }
        _ => done(Err(format!("MCP control {}.{} is unavailable", call.control, call.method))),
    }
}
