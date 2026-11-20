//! A committed output opened as this editor's first book. The renderer's
//! `epub:consume-pending` and `epub:open-file` wait at the barrier, and the
//! book it opens is parsed from the retained bytes, never the reopened path.

use super::*;
use crate::saved_slots::{self, Answer, SavedHost, SavedSlots};
use main_core::packages::electron_utils::save_conflict::record_disk_state;
use shell_contract::ipc::{NativeRequestOwner, NativeSavedDocument};
use shell_contract::runtime::{CancelToken, RendererGeneration};

/// What the renderer's held requests ask for.
pub(super) enum Kind {
    Pending,
    Open(String),
}

/// The parsed book the open request answers with.
#[derive(Clone, Default)]
pub(super) struct Data {
    session: Option<Rc<RefCell<Session>>>,
}

impl Epub {
    /// The document lease of `view` joined to a renderer owner.
    pub(super) fn capture_document_owner(
        &self,
        view: ViewId,
        mut native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError> {
        let document = self
            .view(view, |entry| entry.document.clone())
            .flatten()
            .ok_or_else(|| kit::error("The book document is unavailable"))?;
        if native.view != view || !document.is_active() {
            return Err(kit::error("The book document was released"));
        }
        if !native
            .context
            .renderer()
            .zip(self.services.views.renderer_generation(view).as_ref())
            .is_some_and(|(expected, current)| {
                expected.is_active() && expected.same_generation(current)
            })
            || !native.loss_token().is_some_and(|loss| !loss.is_cancelled())
        {
            return Err(kit::error("The book renderer was replaced"));
        }
        if let Some(expected) = native.context.document_lease()
            && !expected.same_generation(&document)
        {
            return Err(kit::error("The book document was replaced"));
        }
        native.context = native.context.with_document_lease(document);
        Ok(native)
    }
}

impl SavedHost for Epub {
    type Lease = RendererGeneration;
    type Kind = Kind;
    type Data = Data;

    fn surface() -> Surface {
        Surface::Epub
    }

    fn services(&self) -> &Services {
        &self.services
    }

    fn slots(&self) -> &SavedSlots<Self> {
        &self.saved
    }

    fn lease(&self, view: ViewId) -> Option<RendererGeneration> {
        self.view(view, |entry| entry.document.clone())
            .flatten()
            .filter(RendererGeneration::is_active)
    }

    fn lease_current(&self, view: ViewId, lease: &RendererGeneration) -> bool {
        self.view(view, |entry| {
            entry
                .document
                .as_ref()
                .is_some_and(|current| current.is_active() && current.same_generation(lease))
        })
        .unwrap_or(false)
    }

    fn lease_loss(lease: &RendererGeneration) -> CancelToken {
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
        view: ViewId,
        kind: &Kind,
        answer: &Answer<'_, Data>,
    ) -> Result<(IpcValue, bool), String> {
        match kind {
            Kind::Pending => Ok((
                object(vec![
                    ("path", answer.path.into_ipc()),
                    ("receipt", answer.token.into_ipc()),
                ]),
                false,
            )),
            Kind::Open(requested) => {
                if requested != answer.path || answer.applying {
                    return Err("The saved book open does not match its target".into());
                }
                let session = answer
                    .data
                    .session
                    .clone()
                    .ok_or("The saved book was not parsed")?;
                Ok((self.opened(view, answer.path, &session), true))
            }
        }
    }

    fn settled(&self, view: ViewId, ok: bool, _error: &str) {
        if !ok {
            self.refuse_writes(view);
        }
    }
}

/// Parses the retained bytes off the core, then installs the book as the
/// view's own and lets the held requests through.
pub(super) fn install(
    epub: &Rc<Epub>,
    document: NativeSavedDocument,
    lease_owner: NativeRequestOwner,
) {
    let target = document.target().clone();
    let view = target.view();
    let path = match saved_slots::path_of(&target) {
        Ok(path) => path.to_owned(),
        Err(error) => return epub.saved.fail(&target, &error),
    };
    let bytes: Bytes = Arc::new(target.input().bytes().to_vec());
    let engine = epub.engine.clone();
    let stat_path = path.clone();
    let host = Rc::clone(epub);
    kit::on_worker(
        &epub.services,
        move || {
            let opened = engine.open(bytes.clone())?;
            Ok::<_, String>((bytes, opened, record_disk_state(&stat_path)))
        },
        move |result| {
            let (bytes, opened, disk) = match result {
                Ok(parsed) => parsed,
                Err(reason) => {
                    return host
                        .saved
                        .fail(&target, &format!("The saved book did not open ({reason})"));
                }
            };
            let session = Rc::new(RefCell::new(Session::new(bytes, opened)));
            let data = Data {
                session: Some(session.clone()),
            };
            let installed =
                saved_slots::installed(&host, &document, &lease_owner, data, &mut || {
                    host.with_view(view, |entry| {
                        entry.session = Some(session.clone());
                        entry.open_path = Some(path.clone());
                        entry.allowed = HashSet::from([path.clone()]);
                        if let Some(disk) = &disk {
                            entry.disk_states.insert(path.clone(), disk.clone());
                        }
                    })
                    .ok_or_else(|| "The book view was released".to_owned())
                });
            if let Err(error) = installed {
                host.saved.fail(&target, &error);
            }
        },
    );
}
