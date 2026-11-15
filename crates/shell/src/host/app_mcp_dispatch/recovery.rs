use std::rc::Rc;

use jscompat::JsonValue;
use main_core::shell::mcp::mcp_server::{HostCall, Settled};
use shell_contract::ids::ViewId;
use shell_contract::provided::Provided;
use shell_contract::runtime::Services;
use shell_contract::Surface;

use super::visible::view_id;

pub(super) struct Recovery {
    docs: Option<Rc<shell_ipc_d2::docs::editors::DocsEditors>>,
    pdf: Option<Rc<shell_ipc_d2::pdf::editor::editors::PdfEditors>>,
    hwp: Option<Rc<shell_ipc_d2::hwp::HwpEditors>>,
}

impl Recovery {
    pub(super) fn new(provided: &Provided) -> Self {
        Self {
            docs: provided.all::<shell_ipc_d2::docs::editors::DocsEditors>().into_iter().next(),
            pdf: provided.all::<shell_ipc_d2::pdf::editor::editors::PdfEditors>().into_iter().next(),
            hwp: provided.all::<shell_ipc_d2::hwp::HwpEditors>().into_iter().next(),
        }
    }

    pub(super) fn dispatch(&self, services: &Services, call: HostCall) -> Settled {
        let (kind, view) = match call.method {
            "slidesDiscard" => ("slides".to_owned(), view_id(call.args.first())?),
            "recoveryDiscard" => {
                let kind = match call.args.first().and_then(|tab| tab.get("kind")).as_deref() {
                    Some(JsonValue::String(kind)) => kind.clone(),
                    _ => return Err("invalid document kind".to_owned()),
                };
                (kind, view_id(call.args.get(1))?)
            }
            _ => return Err(format!("unknown recovery action {}", call.method)),
        };
        let view = ViewId(u32::try_from(view).map_err(|_| "invalid document view id".to_owned())?);
        let Some(sender) = services.views.sender_of(view) else {
            return Err("the document view is closed".to_owned());
        };
        let expected = match kind.as_str() {
            "docs" => Surface::Docs,
            "sheets" => Surface::Sheets,
            "slides" => Surface::Slides,
            "pdf" => Surface::Pdf,
            "mindmap" => Surface::Mindmap,
            "hwp" => Surface::Hwp,
            "diagram" => Surface::Diagram,
            "epub" => Surface::Epub,
            _ => return Err("invalid document kind".to_owned()),
        };
        if sender.surface != expected {
            return Err("the document view belongs to another editor".to_owned());
        }
        match kind.as_str() {
            "docs" => self.docs.as_ref().ok_or("Word editor is unavailable")?.discard_recovery(view),
            "sheets" => shell_ipc_d1::sheets::editors::views().ok_or("Sheets editor is unavailable")?.discard_recovery(view),
            "slides" => shell_ipc_d1::slides::views().ok_or("Slides editor is unavailable")?.discard_recovery(view),
            "pdf" => self.pdf.as_ref().ok_or("PDF editor is unavailable")?.discard_recovery(view),
            "mindmap" => shell_ipc_d1::mindmap::handle().ok_or("Mindmap editor is unavailable")?.discard_recovery(view),
            "hwp" => self.hwp.as_ref().ok_or("HWP editor is unavailable")?.discard_recovery_copy(view),
            "diagram" => shell_ipc_d1::diagram::views().ok_or("Diagram editor is unavailable")?.discard_recovery(view),
            "epub" => (),
            _ => unreachable!(),
        }
        Ok(None)
    }
}
