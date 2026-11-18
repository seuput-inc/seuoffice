use main_core::shell::tab_manager::ViewClosureWatch;
use shell_contract::{ViewId, ipc::{NativeBrowserOwner, NativeBrowserRetirement, NativeClosureReservation}, runtime::RendererGeneration};
use crate::tabs::{state::TabsState, host::EditorSeedRollbackStore};

pub(crate) struct AdoptedEditorLifetime {
    view: ViewId,
    created: RendererGeneration,
    reservation: NativeClosureReservation,
    owner: NativeBrowserOwner,
    watch: ViewClosureWatch<NativeBrowserRetirement>,
    rollback: EditorSeedRollbackStore,
    receipt: Option<NativeBrowserRetirement>,
    watching: bool,
    retiring: bool,
}

impl TabsState {
    pub fn retain_adopted_editor(
        &self, view: ViewId, created: RendererGeneration, reservation: NativeClosureReservation,
        owner: NativeBrowserOwner, watch: ViewClosureWatch<NativeBrowserRetirement>, rollback: EditorSeedRollbackStore,
    ) -> Result<(), String> {
        if !reservation.matches(view, &created) || !owner.reservation().same_reservation(&reservation) { return Err("The adopted editor reservation does not match its view".into()); }
        let id = self.owned_id()?;
        self.adopted_editors.borrow_mut().insert(id, AdoptedEditorLifetime {
            view, created, reservation, owner, watch, rollback, receipt: None, watching: false, retiring: false,
        });
        self.retry_adopted_editor(id)?;
        Ok(())
    }

    fn retry_adopted_editor(&self, id: u64) -> Result<(), String> {
        let watch = {
            let mut entries = self.adopted_editors.borrow_mut();
            let entry = entries.get_mut(&id).ok_or("The adopted editor lifetime is unavailable")?;
            if entry.receipt.is_some() { drop(entries); return self.finish_adopted_editor(id); }
            if entry.watching { return Ok(()); }
            entry.watching = true;
            entry.watch.clone()
        };
        let state = self.rc();
        let callback = crate::home_files::back_to_core(self.services.core.clone(), move |receipt: NativeBrowserRetirement| {
            let valid = {
                let mut entries = state.adopted_editors.borrow_mut();
                let Some(entry) = entries.get_mut(&id) else { return; };
                if entry.receipt.is_some() { return; }
                entry.watching = false;
                let matches = match &receipt {
                    NativeBrowserRetirement::Closed(proof) => proof.matches(&entry.reservation, entry.view, &entry.created),
                    NativeBrowserRetirement::Superseded(proof) => proof.matches(&entry.reservation, &entry.owner),
                };
                if !matches { false }
                else { entry.receipt = Some(receipt); true }
            };
            let result = if valid { state.finish_adopted_editor(id) } else { Err("The native closure receipt does not match the adopted editor".into()) };
            if let Err(error) = result { state.hooks.warn("[shell] adopted editor retirement is pending:", &error); }
        });
        if let Err(error) = watch(callback) {
            if let Some(entry) = self.adopted_editors.borrow_mut().get_mut(&id) { entry.watching = false; }
            return Err(error);
        }
        Ok(())
    }

    fn finish_adopted_editor(&self, id: u64) -> Result<(), String> {
        let (proof, store) = {
            let mut entries = self.adopted_editors.borrow_mut();
            let entry = entries.get_mut(&id).ok_or("The adopted editor lifetime is unavailable")?;
            if entry.retiring { return Ok(()); }
            let proof = entry.receipt.clone().ok_or("The native closure has not been acknowledged")?;
            entry.retiring = true;
            (proof, entry.rollback.clone())
        };
        let state = self.rc();
        let done = Box::new(move |result| {
            match result {
                Ok(()) => { state.adopted_editors.borrow_mut().remove(&id); }
                Err(error) => {
                    if let Some(entry) = state.adopted_editors.borrow_mut().get_mut(&id) { entry.retiring = false; }
                    state.hooks.warn("[shell] adopted editor retirement is pending:", &error);
                }
            }
        });
        match proof {
            NativeBrowserRetirement::Closed(proof) => super::receipt_retirement::retire_adopted_editor_receipts(store, proof, done),
            NativeBrowserRetirement::Superseded(proof) => super::receipt_retirement::rollback_editor_receipts(store, Some(NativeBrowserRetirement::Superseded(proof)), true, done),
        }
        Ok(())
    }

    pub(crate) fn supersede_adopted_editor(
        &self, reservation: &NativeClosureReservation, owner: &NativeBrowserOwner,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        if !proof.matches(reservation, owner) { return Err("The browser replacement does not match its owner".into()); }
        let id = {
            let mut entries = self.adopted_editors.borrow_mut();
            let (id, entry) = entries.iter_mut().find(|(_, entry)|
                entry.reservation.same_reservation(reservation) && entry.owner.same_owner(owner))
                .ok_or("The replaced editor lifetime is unavailable")?;
            if entry.receipt.is_some() { return Err("The editor retirement was already acknowledged".into()); }
            entry.receipt = Some(NativeBrowserRetirement::Superseded(proof));
            *id
        };
        self.finish_adopted_editor(id)
    }

    pub fn retry_adopted_editor_lifetimes(&self) -> Result<(), String> {
        let ids = self.adopted_editors.borrow().keys().copied().collect::<Vec<_>>();
        for id in ids { self.retry_adopted_editor(id)?; }
        Ok(())
    }
}
