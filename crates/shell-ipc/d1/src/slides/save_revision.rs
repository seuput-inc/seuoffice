use super::SaveScope;
use crate::slides::state::App;
use crate::value::thrown;
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::atomic_write::owned::Admission;
use shell_contract::IpcError;
use std::sync::{Arc, mpsc};

#[derive(Clone)]
pub(in crate::slides) struct SaveRevision {
    pub path: String,
    pub meta_rev: Option<u64>,
    pub op_seq: Option<u64>,
}

impl SaveScope {
    pub(in crate::slides) fn revision(&self, app: &App) -> Result<SaveRevision, IpcError> {
        self.check(app)?;
        let state = app.state.borrow();
        let s = state
            .slot(self.slot)
            .ok_or_else(|| thrown("The presentation was released"))?;
        Ok(SaveRevision {
            path: s.path().to_owned(),
            meta_rev: s.core.meta_rev,
            op_seq: s.core.op_seq,
        })
    }

    pub(in crate::slides) fn check_revision_state(
        &self,
        app: &App,
        revision: &SaveRevision,
    ) -> Result<(), IpcError> {
        self.check_document(app)?;
        let state = app.state.borrow();
        let s = state
            .slot(self.slot)
            .ok_or_else(|| thrown("The presentation was released"))?;
        if s.core.ai_run
            || s.path() != revision.path
            || s.core.meta_rev != revision.meta_rev
            || s.core.op_seq != revision.op_seq
        {
            return Err(thrown(
                "The presentation changed while the operation was pending",
            ));
        }
        Ok(())
    }

    pub(in crate::slides) fn effect_at_revision<T>(
        &self,
        app: &App,
        revision: &SaveRevision,
        effect: impl FnOnce() -> T,
    ) -> Result<T, IpcError> {
        self.effect(app, || {
            {
                let state = app.state.borrow();
                let s = state
                    .slot(self.slot)
                    .ok_or_else(|| thrown("The presentation was released"))?;
                if s.core.ai_run
                    || s.path() != revision.path
                    || s.core.meta_rev != revision.meta_rev
                    || s.core.op_seq != revision.op_seq
                {
                    return Err(thrown(
                        "The presentation changed while the operation was pending",
                    ));
                }
            }
            Ok(effect())
        })
    }

    pub(in crate::slides) fn revision_admission(
        &self,
        app: &App,
        revision: SaveRevision,
    ) -> Admission {
        let scope = self.clone();
        let core = app.services.core.clone();
        let index = app.registry_index;
        Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1);
            let scope = scope.clone();
            let revision = revision.clone();
            core.post(Box::new(move || {
                let answer = crate::slides::with_app(index, |app| {
                    scope.effect_at_revision(app, &revision, effect)
                })
                .unwrap_or_else(|| Err(thrown("The presentation application was released")));
                let _ = send.send(answer.map_err(|error| NodeError {
                    code: None,
                    message: error.message,
                }));
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The document operation dispatcher was released".into(),
            })?
        })
    }

    pub(in crate::slides) fn revision_destination_admission(
        &self,
        app: &App,
        revision: SaveRevision,
        path: String,
    ) -> Admission {
        let scope = self.clone();
        let core = app.services.core.clone();
        let index = app.registry_index;
        Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1);
            let scope = scope.clone();
            let revision = revision.clone();
            let path = path.clone();
            core.post(Box::new(move || {
                let answer =
                    crate::slides::with_app(index, |app| match &scope.native_destination {
                        Some(destination) => scope.request.effect_destination(
                            destination,
                            std::path::Path::new(&path),
                            || {
                                scope.check_revision_state(app, &revision)?;
                                Ok(effect())
                            },
                        ),
                        None => scope.effect_at_revision(app, &revision, effect),
                    })
                    .unwrap_or_else(|| Err(thrown("The presentation application was released")));
                let _ = send.send(answer.map_err(|error| NodeError {
                    code: None,
                    message: error.message,
                }));
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The document operation dispatcher was released".into(),
            })?
        })
    }

    pub(in crate::slides) fn revision_numbered_admission(
        &self,
        app: &App,
        revision: SaveRevision,
        path: String,
        count: usize,
        index: usize,
    ) -> Admission {
        let scope = self.clone();
        let core = app.services.core.clone();
        let registry = app.registry_index;
        Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1);
            let scope = scope.clone();
            let revision = revision.clone();
            let path = path.clone();
            core.post(Box::new(move || {
                let answer = crate::slides::with_app(registry, |app| {
                    let Some(destination) = &scope.native_destination else {
                        if matches!(&scope.request.authority, crate::owned_save::SaveAuthority::Native(_)) { return Err(thrown("The native image destination was not retained")); }
                        return scope.effect_at_revision(app, &revision, effect);
                    };
                    if !matches!(&scope.request.authority, crate::owned_save::SaveAuthority::Native(binding) if destination.matches_binding(binding)) {
                        return Err(thrown("The image destination belongs to another save"));
                    }
                    let page = destination.numbered_png_page(count, index).map_err(thrown)?;
                    let participants = scope.request.participants(&[]);
                    let additional = scope.request.additional.iter().collect::<Vec<_>>();
                    let mut effect = Some(effect); let mut result = None;
                    scope.request.effects.with_numbered_save_destination_effect(&page, std::path::Path::new(&path), &participants, &scope.request.cancel, &scope.request.pending, &additional, &mut || {
                        scope.check_revision_state(app, &revision).map_err(|error| error.message)?;
                        let run = effect.take().ok_or("The image write was already consumed")?;
                        result = Some(run());
                        Ok(())
                    }).map_err(thrown)?;
                    result.ok_or_else(|| thrown("The image write was not admitted"))
                }).unwrap_or_else(|| Err(thrown("The presentation application was released")));
                let _ = send.send(answer.map_err(|error| NodeError { code: None, message: error.message }));
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The document operation dispatcher was released".into(),
            })?
        })
    }
}
