//! A committed output opened as this editor's first document. The renderer's
//! `hwp:consume-recovery` waits at the barrier, its `hwp:read-file` is
//! answered from the retained bytes, never the reopened path, and
//! `hwp:recovery-initialized` is its receipt.

use super::*;
use crate::saved_slots::{self, Answer, SavedHost, SavedRead, SavedSlots};
use shell_contract::ipc::{NativeRequestOwner, NativeSavedDocument};
use shell_contract::runtime::{CancelToken, RendererGeneration};

impl SavedHost for Hwp {
    type Lease = RendererGeneration;
    type Kind = ();
    type Data = ();

    fn surface() -> shell_contract::Surface {
        shell_contract::Surface::Hwp
    }

    fn services(&self) -> &Services {
        &self.services
    }

    fn slots(&self) -> &SavedSlots<Self> {
        &self.saved
    }

    fn lease(&self, view: ViewId) -> Option<RendererGeneration> {
        self.get(view, |state| state.document_epoch.clone())
            .filter(RendererGeneration::is_active)
    }

    fn lease_current(&self, view: ViewId, lease: &RendererGeneration) -> bool {
        self.get(view, |state| {
            state
                .document_epoch
                .as_ref()
                .is_some_and(|current| current.is_active() && current.same_generation(lease))
        })
    }

    fn lease_loss(lease: &RendererGeneration) -> CancelToken {
        lease.loss_token()
    }

    fn capture_owner(
        &self,
        view: ViewId,
        native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError> {
        self.capture_hwp_owner(view, native)
    }

    fn answer(
        &self,
        view: ViewId,
        _kind: &(),
        answer: &Answer<'_, ()>,
    ) -> Result<(IpcValue, bool), String> {
        self.recovery.borrow_mut().note_hwp_path_consumed(wc(view));
        Ok((
            obj([
                ("path", answer.path.into_ipc()),
                ("token", answer.token.into_ipc()),
            ]),
            true,
        ))
    }

    fn settled(&self, _view: ViewId, _ok: bool, _error: &str) {}
}

/// The path becomes the view's own file and the held requests go through.
pub(super) fn install(
    hwp: &Rc<Hwp>,
    document: NativeSavedDocument,
    lease_owner: NativeRequestOwner,
) {
    let target = document.target().clone();
    let view = target.view();
    let path = match saved_slots::path_of(&target) {
        Ok(path) => path.to_owned(),
        Err(message) => return hwp.saved.fail(&target, &message),
    };
    let installed = saved_slots::installed(hwp, &document, &lease_owner, (), &mut || {
        hwp.change(view, |state| {
            state.open_path = Some(path.clone());
            state.allowed = HashSet::from([path.clone()]);
            state.save_path = Some(path.clone());
        });
        Ok(())
    });
    if let Err(message) = installed {
        hwp.saved.fail(&target, &message);
    }
}

impl Hwp {
    /// `hwp:read-file` of the saved path: the engine reads the retained bytes.
    pub(super) fn read_saved_file(
        self: &Rc<Self>,
        view: ViewId,
        path: String,
        scope: SavedRead,
        reply: shell_contract::ipc::Responder,
    ) {
        let engine = Arc::clone(&self.engine);
        let input = scope.input().clone();
        let worker_path = path.clone();
        let hwp = Rc::clone(self);
        on_worker(
            &self.services,
            move || {
                let bytes = input.bytes();
                if bytes.len() > MAX_HWP_SAVE_BYTES {
                    return Err(error("hwp: file is too large to open"));
                }
                read_hwp_bytes(&engine, bytes.to_vec(), record_disk_state(&worker_path))
            },
            move |read| {
                let mut read = match read {
                    Ok(read) => Some(read),
                    Err(failure) => return reply.reply(Err(failure)),
                };
                let mut bytes = None;
                let admitted = scope.admit(&mut || {
                    let read = read.take().ok_or("HWP saved input was already delivered")?;
                    if hwp.is_destroyed(view) {
                        return Err("hwp: document destroyed".into());
                    }
                    bytes = Some(read.bytes);
                    hwp.change(view, |state| {
                        if let Some(disk) = read.disk {
                            state.disk_states.insert(path.clone(), disk);
                        }
                        state.tracked = read.tracked;
                        state.tracking_dropped = read.dropped;
                        state.unverified = read.unverified.then(|| path.clone());
                        state.base = read.base;
                    });
                    Ok(())
                });
                match (admitted, bytes) {
                    (Ok(()), Some(bytes)) => reply.reply(Ok(u8_array(bytes))),
                    (Err(message), _) => reply.reply(Err(error(message))),
                    _ => reply.reply(Err(error("HWP saved input delivery was not admitted"))),
                }
            },
        );
    }
}
