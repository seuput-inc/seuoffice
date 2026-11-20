//! A committed output opened as this editor's first document. The renderer's
//! `pdf:consume-recovery-pending` waits at the barrier, and its
//! `pdf:read-file` is answered from the retained bytes, never the reopened
//! path; `pdf:recovery-initialized` is the renderer's receipt.

use std::rc::Rc;

use main_core::packages::electron_utils::save_conflict::record_disk_state;
use shell_contract::ipc::{NativeRequestOwner, NativeSavedDocument, PdfDocumentLease};
use shell_contract::runtime::{CancelToken, Services};
use shell_contract::{IpcError, IpcValue, Surface, ViewId};

use super::Editor;
use crate::kit::{self, IntoIpc, error};
use crate::saved_slots::{self, Answer, SavedHost, SavedSlots};

impl Editor {
    /// The view's document lease and revision joined to a renderer owner.
    pub(super) fn capture_document_owner(
        &self,
        view: ViewId,
        native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError> {
        if native.view != view {
            return Err(error("pdf: document owner mismatch"));
        }
        let expected = self
            .services
            .views
            .renderer_generation(view)
            .ok_or_else(|| error("pdf: renderer unavailable"))?;
        if !native
            .context
            .renderer()
            .is_some_and(|renderer| renderer.is_active() && renderer.same_generation(&expected))
        {
            return Err(error("pdf: renderer expired"));
        }
        let state = self.st.borrow();
        let slot = state
            .views
            .get(&view)
            .ok_or_else(|| error("pdf: document unavailable"))?;
        let lease = slot
            .document_lease
            .as_ref()
            .filter(|lease| lease.is_live())
            .ok_or_else(|| error("pdf: document unavailable"))?;
        let revision = slot
            .document_revision
            .as_ref()
            .filter(|revision| revision.is_live())
            .ok_or_else(|| error("pdf: document unavailable"))?;
        if native
            .pdf_document()
            .is_some_and(|captured| !captured.same_document(lease))
            || native
                .pdf_revision()
                .is_some_and(|captured| !captured.same_document(revision))
        {
            return Err(error("pdf: document changed"));
        }
        if native.pdf_document().is_some() && native.pdf_revision().is_some() {
            return Ok(native);
        }
        let context = native
            .context
            .with_pdf_document(lease.clone())
            .with_pdf_revision(revision.clone());
        Ok(NativeRequestOwner { view, context })
    }
}

impl SavedHost for Editor {
    type Lease = PdfDocumentLease;
    type Kind = ();
    type Data = ();

    fn surface() -> Surface {
        Surface::Pdf
    }

    fn services(&self) -> &Services {
        &self.services
    }

    fn slots(&self) -> &SavedSlots<Self> {
        &self.saved
    }

    fn lease(&self, view: ViewId) -> Option<PdfDocumentLease> {
        self.st
            .borrow()
            .views
            .get(&view)?
            .document_lease
            .clone()
            .filter(PdfDocumentLease::is_live)
    }

    fn lease_current(&self, view: ViewId, lease: &PdfDocumentLease) -> bool {
        self.st
            .borrow()
            .views
            .get(&view)
            .and_then(|slot| slot.document_lease.as_ref())
            .is_some_and(|current| current.is_live() && current.same_document(lease))
    }

    fn lease_loss(lease: &PdfDocumentLease) -> CancelToken {
        lease.loss_token()
    }

    fn capture_owner(
        &self,
        view: ViewId,
        native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError> {
        self.capture_document_owner(view, native)
    }

    fn answer(
        &self,
        _view: ViewId,
        _kind: &(),
        answer: &Answer<'_, ()>,
    ) -> Result<(IpcValue, bool), String> {
        Ok((
            kit::obj([
                ("path", answer.path.into_ipc()),
                ("token", answer.token.into_ipc()),
            ]),
            true,
        ))
    }

    fn settled(&self, _view: ViewId, _ok: bool, _error: &str) {}
}

/// The path becomes the view's own file and the held requests go through.
/// Nothing is parsed here: the renderer's pdf.js parses the retained bytes.
pub(super) fn install(
    editor: &Rc<Editor>,
    document: NativeSavedDocument,
    lease_owner: NativeRequestOwner,
) {
    let target = document.target().clone();
    let view = target.view();
    let path = match saved_slots::path_of(&target) {
        Ok(path) => path.to_owned(),
        Err(message) => return editor.saved.fail(&target, &message),
    };
    let host = Rc::clone(editor);
    let stat_path = path.clone();
    kit::on_worker(
        &editor.services,
        move || record_disk_state(&stat_path),
        move |disk| {
            let installed = saved_slots::installed(&host, &document, &lease_owner, (), &mut || {
                host.grant_only(view, &path);
                host.with_view(view, |slot| slot.open_path = Some(path.clone()));
                host.remember_disk_state(view, &path, disk.clone());
                Ok(())
            });
            if let Err(message) = installed {
                host.saved.fail(&target, &message);
            }
        },
    );
}
