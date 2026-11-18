use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::runtime::{CancelToken, RendererGeneration};
use shell_contract::IpcError;
use crate::value::thrown;
use super::engine::RenderOwner;
use super::state::{App, SlideSession, StagedBinding};

#[derive(Clone)]
pub(super) struct OpenAuthority {
    pub source: RenderOwner,
    pub cancel: CancelToken,
}

impl OpenAuthority {
    pub fn capture(app: &App, raw: &RenderOwner, cancel: CancelToken) -> Result<Self, IpcError> {
        let source = if app.state.borrow().has_session(raw.view) { app.capture_save_owner(raw.view, raw.clone())? } else {
            if raw.context.document_lease().is_some() { return Err(thrown("The presentation open owner refers to a released document")); }
            raw.clone()
        };
        if cancel.is_cancelled() { return Err(thrown("The presentation open request was cancelled")); }
        Ok(Self { source, cancel })
    }

    pub fn commit(&self, app: &App, raw: &RenderOwner, slot: Option<usize>, session: Option<SlideSession>, staged: StagedBinding) -> Result<RenderOwner, IpcError> {
        let effects = app.services.hosts.get::<OwnedEffectsHost>().cloned().ok_or_else(|| thrown("The native document transition host is unavailable"))?;
        let previous = app.state.borrow().binding_of(raw.view);
        let slot = match slot { Some(slot) => slot, None => app.state.borrow_mut().reserve_open_slot() };
        let expected = self.source.context.document_lease().cloned();
        let mut successor = if expected.is_some() { Some(self.source.prepare_document_successor().map_err(thrown)?) } else { None };
        let mut initial_owner = raw.clone();
        if expected.is_none() { initial_owner.context = initial_owner.context.with_document_lease(RendererGeneration::new()); }
        let mut replacement = session;
        let mut retired = None;
        let mut completed_owner = None;
        let result = effects.with_owned_transition(&self.source, &self.source, &self.cancel, &self.cancel, &mut |retirement| {
            if retired.is_some() { return Err("The presentation transition was already applied".into()); }
            let (old, owner) = app.state.borrow_mut().attach_retiring(initial_owner.clone(), slot, replacement.take(), successor.take(), expected.as_ref(), retirement)?;
            retired = Some(old);
            completed_owner = Some(owner);
            Ok(())
        }).map_err(thrown);
        let result = result.and_then(|()| staged.commit(previous).map_err(|error| thrown(error.0)));
        drop(retired);
        result?;
        completed_owner.ok_or_else(|| thrown("The presentation transition did not publish its owner"))
    }

    pub fn commit_saved(&self, app: &App, raw: &RenderOwner, document: &shell_contract::ipc::NativeSavedDocument, session: SlideSession, staged: StagedBinding) -> Result<RenderOwner, IpcError> {
        let effects = app.services.hosts.get::<OwnedEffectsHost>().cloned().ok_or_else(|| thrown("The native document transition host is unavailable"))?;
        let previous = app.state.borrow().binding_of(raw.view);
        let slot = app.state.borrow_mut().reserve_open_slot();
        let expected = if previous.is_some() { self.source.context.document_lease().cloned() } else { None };
        let mut successor = if expected.is_some() { Some(self.source.prepare_document_successor().map_err(thrown)?) } else { None };
        let initial_owner = if expected.is_none() { document.owner().clone() } else { raw.clone() };
        let mut replacement = Some(session);
        let mut retired = None;
        let mut completed_owner = None;
        let target = document.target();
        let extra = target.additional().iter().collect::<Vec<_>>();
        let result = effects.with_native_saved_document_transition(document, raw, &self.cancel, &[self.source.clone()], target.cancel(), target.pending(), &extra, &mut |_, retirement| {
            if retired.is_some() { return Err("The presentation transition was already applied".into()); }
            let (old, owner) = app.state.borrow_mut().attach_retiring(initial_owner.clone(), slot, replacement.take(), successor.take(), expected.as_ref(), retirement)?;
            retired = Some(old);
            completed_owner = Some(owner);
            Ok(())
        }).map_err(thrown);
        let result = result.and_then(|()| staged.commit(previous).map_err(|error| thrown(error.0)));
        drop(retired);
        result?;
        completed_owner.ok_or_else(|| thrown("The presentation transition did not publish its owner"))
    }
}
