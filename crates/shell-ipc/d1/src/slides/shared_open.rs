use super::engine::{DeckId, RenderOwner, Snapshot};
use super::open_authority::OpenAuthority;
use super::owned_save::SaveRevision;
use super::state::{App, SlideSession};
use crate::value::thrown;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{NativeRequestOwner, PreparedDocumentOwner};
use shell_contract::runtime::CancelToken;
use shell_contract::{IpcError, ViewId};

pub(super) struct SharedOpen {
    pub source: RenderOwner,
    pub slot: Option<usize>,
    pub session: Option<u64>,
    pub deck: Option<DeckId>,
    pub revision: Option<SaveRevision>,
    pub participants: Vec<RenderOwner>,
    pub ids: Vec<ViewId>,
    pub successors: Vec<Option<PreparedDocumentOwner>>,
    pub cancel: CancelToken,
}

impl SharedOpen {
    pub fn capture(app: &App, owner: &RenderOwner, cancel: CancelToken) -> Result<Self, IpcError> {
        let source = OpenAuthority::capture(app, owner, cancel.clone())?.source;
        let state = app.state.borrow();
        let slot = state.slot_of(owner.view);
        let session = slot.and_then(|slot| state.slot(slot));
        let revision = session.map(|s| SaveRevision {
            path: s.path().to_owned(),
            meta_rev: s.core.meta_rev,
            op_seq: s.core.op_seq,
        });
        let participants = if let Some(slot) = slot {
            state
                .attached_owners(slot)
                .into_iter()
                .map(|mut participant| {
                    let document = state
                        .save_lifetime(participant.view)
                        .ok_or_else(|| thrown("An attached presentation was released"))?
                        .clone();
                    if participant
                        .context
                        .document_lease()
                        .is_some_and(|held| !held.same_generation(&document))
                    {
                        return Err(thrown("An attached presentation was replaced"));
                    }
                    if participant.context.document_lease().is_none() {
                        participant.context = participant.context.with_document_lease(document);
                    }
                    Ok(participant)
                })
                .collect::<Result<Vec<_>, IpcError>>()?
        } else {
            Vec::new()
        };
        let ids = participants.iter().map(|owner| owner.view).collect();
        let successors = participants
            .iter()
            .map(|owner| owner.prepare_document_successor().map(Some).map_err(thrown))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            source,
            slot,
            session: session.map(|s| s.id),
            deck: session.map(|s| s.deck()),
            revision,
            participants,
            ids,
            successors,
            cancel,
        })
    }

    pub fn commit(
        mut self,
        app: &App,
        raw: &RenderOwner,
        mut replacement: SlideSession,
        snapshot: Option<Snapshot>,
        finalize: impl FnOnce(&mut SlideSession),
    ) -> Result<(usize, RenderOwner), IpcError> {
        if self.slot.is_none() {
            finalize(&mut replacement);
            let staged =
                super::state::StagedBinding::new(app.engine.clone(), replacement.deck(), raw)
                    .map_err(|error| thrown(error.0))?;
            let authority = OpenAuthority {
                source: self.source,
                cancel: self.cancel,
            };
            let owner = authority.commit(app, raw, None, Some(replacement), staged)?;
            let slot = app
                .state
                .borrow()
                .slot_of(owner.view)
                .ok_or_else(|| thrown("The generated presentation was released"))?;
            return Ok((slot, owner));
        }
        let effects = app
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| thrown("The native presentation transition host is unavailable"))?;
        let new_slot = app.state.borrow_mut().reserve_open_slot();
        let mut replacement = Some(replacement);
        let mut snapshot = snapshot;
        let mut finalize = Some(finalize);
        let mut retired = Vec::with_capacity(self.participants.len());
        let mut retired_session = None;
        let mut completed = Vec::<NativeRequestOwner>::with_capacity(self.participants.len());
        let participants = std::mem::take(&mut self.participants);
        let source = self.source.clone();
        let cancel = self.cancel.clone();
        let result = effects
            .with_owned_transition_many(
                &source,
                &participants,
                &cancel,
                &cancel,
                &mut |retirement| {
                    app.state.borrow_mut().replace_shared_retiring(
                        &mut self,
                        &participants,
                        new_slot,
                        &mut replacement,
                        &mut snapshot,
                        &mut finalize,
                        &mut retired,
                        &mut retired_session,
                        &mut completed,
                        retirement,
                    )
                },
            )
            .map_err(thrown);
        drop(retired);
        drop(retired_session);
        result?;
        let owner = completed
            .into_iter()
            .find(|owner| owner.view == raw.view)
            .ok_or_else(|| thrown("The generated presenter owner was not published"))?;
        Ok((new_slot, owner))
    }
}
