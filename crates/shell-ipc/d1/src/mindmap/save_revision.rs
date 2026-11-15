use crate::mindmap::app::MindmapApp;
use crate::owned_save::OwnedSave;
use crate::value::thrown;
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::atomic_write::owned::Admission;
use shell_contract::IpcError;
use std::sync::{Arc, mpsc};

#[derive(Clone)]
pub(in crate::mindmap) struct MapRevision {
    pub revision: u64,
    pub path: Option<String>,
}

impl MapRevision {
    pub fn capture(app: &MindmapApp, request: &OwnedSave) -> Result<Self, IpcError> {
        request.check()?;
        let state = app.0.state.borrow();
        let view = request.destination.view;
        if !state
            .documents
            .get(&view)
            .is_some_and(|document| document.same_generation(&request.document))
        {
            return Err(thrown("The map document was replaced"));
        }
        Ok(Self {
            revision: state
                .revisions
                .get(&view)
                .copied()
                .ok_or_else(|| thrown("The map revision was released"))?,
            path: state.save_path.get(&view).cloned(),
        })
    }

    pub fn effect<T>(
        &self,
        app: &MindmapApp,
        request: &OwnedSave,
        effect: impl FnOnce() -> T,
    ) -> Result<T, IpcError> {
        self.effect_many(app, request, &[], effect)
    }

    pub fn effect_many<T>(
        &self,
        app: &MindmapApp,
        request: &OwnedSave,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        effect: impl FnOnce() -> T,
    ) -> Result<T, IpcError> {
        request.effect_many(participants, || {
            {
                let state = app.0.state.borrow();
                let view = request.destination.view;
                if !state
                    .documents
                    .get(&view)
                    .is_some_and(|document| document.same_generation(&request.document))
                    || state.revisions.get(&view).copied() != Some(self.revision)
                    || state.save_path.get(&view).cloned() != self.path
                {
                    return Err(thrown("The map changed while the operation was pending"));
                }
            }
            Ok(effect())
        })
    }

    pub fn admission(&self, app: &MindmapApp, request: &OwnedSave) -> Admission {
        self.admission_many(app, request, Vec::new())
    }

    pub fn admission_many(
        &self,
        app: &MindmapApp,
        request: &OwnedSave,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    ) -> Admission {
        let core = app.0.services.core.clone();
        let revision = self.clone();
        let request = request.clone();
        Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1);
            let request = request.clone();
            let revision = revision.clone();
            let participants = participants.clone();
            core.post(Box::new(move || {
                let result = crate::mindmap::handle()
                    .ok_or_else(|| thrown("The map application was released"))
                    .and_then(|app| revision.effect_many(&app, &request, &participants, effect));
                let _ = send.send(result.map_err(|error| NodeError {
                    code: None,
                    message: error.message,
                }));
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The map operation dispatcher was released".into(),
            })?
        })
    }
}
