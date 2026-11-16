use super::*;
use main_core::shell::document_links::{
    DocumentLink, DocumentLinksDeps, DocumentLinksFile, DocumentLinksSnapshot, MakeEvent,
    is_freshly_saved_path, read_document_links_file,
};
use main_core::shell::recent_files::metadata;
use shell_contract::{hosts::OwnedEffectsHost, ipc::NativeRequestOwner, runtime::CancelToken};

#[derive(Clone)]
pub(crate) struct SavedLinks {
    origin: NativeRequestOwner,
    destination: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    participants: Vec<NativeRequestOwner>,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
    services: Services,
    menu: Option<LinksMenu>,
    epoch: u64,
    recovery: Option<(shell_contract::ipc::RecoverySeedReceipt, NativeRequestOwner)>,
    existing: Option<shell_contract::ipc::NativeSaveDestination>,
}

#[derive(Clone)]
enum LinksMenu {
    Interactive(super::super::SavedMenu),
    Recovery(super::super::RecoverySavedMenu),
    Existing(super::super::ExistingDocumentSavedMenu),
}

fn admit(
    effects: &OwnedEffectsHost,
    origin: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
    recovery: Option<&(shell_contract::ipc::RecoverySeedReceipt, NativeRequestOwner)>,
    existing: Option<&shell_contract::ipc::NativeSaveDestination>,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if let Some(destination) = existing {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_existing_document_destination_effect(
            destination,
            destination.path(),
            participants,
            cancel,
            pending,
            &extra,
            effect,
        )
    } else if let Some((receipt, receiver)) = recovery {
        effects.with_recovery_open_effect_many(
            receipt,
            receiver,
            participants,
            cancel,
            pending,
            effect,
        )
    } else {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_owned_effect_many_extra(origin, participants, cancel, pending, &extra, effect)
    }
}

#[derive(Clone)]
enum Operation {
    Saved { path: String, may_be_open: bool },
    Moved { old: String, new: String },
}
struct WorkerDeps {
    path: String,
    events: Vec<MakeEvent>,
    writes: RefCell<Vec<DocumentLinksFile>>,
}
impl DocumentLinksDeps for WorkerDeps {
    fn load(&mut self) -> Vec<DocumentLink> {
        let path = self.path.clone();
        metadata::run_sync(&self.path, move || Ok(read_document_links_file(&path)))
            .unwrap_or_default()
    }
    fn save(&mut self, file: &DocumentLinksFile) -> Result<(), String> {
        self.writes.borrow_mut().push(file.clone());
        Ok(())
    }
    fn events(&mut self) -> Vec<MakeEvent> {
        self.events.clone()
    }
    fn now_ms(&mut self) -> f64 {
        now_ms()
    }
    fn warn(&mut self, label: &str, error: &str) {
        eprintln!("{label} {error}");
    }
}
struct Plan {
    state: DocumentLinksSnapshot,
    writes: Vec<DocumentLinksFile>,
}

impl SavedLinks {
    pub(crate) fn capture_many(
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        effects: OwnedEffectsHost,
        services: Services,
    ) -> Result<Self, String> {
        Self::capture_many_extra(
            origin,
            destination,
            participants,
            cancel,
            pending,
            Vec::new(),
            effects,
            services,
        )
    }
    pub(crate) fn capture_many_extra(
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        mut participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        services: Services,
    ) -> Result<Self, String> {
        participants.push(destination.clone());
        let extra: Vec<_> = additional.iter().collect();
        effects.with_owned_effect_many_extra(
            &origin,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || Ok(()),
        )?;
        let menu = super::super::SavedMenu::capture_many_extra(
            &origin,
            &destination,
            &participants,
            &cancel,
            &pending,
            &additional,
            &effects,
        )?
        .map(LinksMenu::Interactive);
        let epoch = LINKS_EPOCH.with(std::cell::Cell::get);
        Ok(Self {
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
            services,
            menu,
            epoch,
            recovery: None,
            existing: None,
        })
    }
    pub(crate) fn capture_recovery(
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        effects: OwnedEffectsHost,
        services: Services,
    ) -> Result<Self, String> {
        effects.with_recovery_open_effect_many(
            &receipt,
            &receiver,
            &participants,
            &cancel,
            &pending,
            &mut || Ok(()),
        )?;
        let menu = super::super::RecoverySavedMenu::capture(
            &receipt, &receiver, &cancel, &pending, &effects,
        )?
        .map(LinksMenu::Recovery);
        let epoch = LINKS_EPOCH.with(std::cell::Cell::get);
        Ok(Self {
            origin: receiver.clone(),
            destination: receiver.clone(),
            participants,
            cancel,
            pending,
            additional: Vec::new(),
            effects,
            services,
            menu,
            epoch,
            recovery: Some((receipt, receiver)),
            existing: None,
        })
    }
    pub(crate) fn capture_existing_document(
        destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        services: Services,
    ) -> Result<Self, String> {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_existing_document_destination_effect(
            &destination,
            destination.path(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || Ok(()),
        )?;
        let menu = super::super::ExistingDocumentSavedMenu::capture(
            destination.binding(),
            &participants,
            &cancel,
            &pending,
            &additional,
            &effects,
        )?
        .map(LinksMenu::Existing);
        let epoch = LINKS_EPOCH.with(std::cell::Cell::get);
        let owner = destination.binding().owner().clone();
        Ok(Self {
            origin: owner.clone(),
            destination: owner,
            participants,
            cancel,
            pending,
            additional,
            effects,
            services,
            menu,
            epoch,
            recovery: None,
            existing: Some(destination),
        })
    }
    fn check(&self) -> Result<(), String> {
        admit(
            &self.effects,
            &self.origin,
            &self.participants,
            &self.cancel,
            &self.pending,
            &self.additional,
            self.recovery.as_ref(),
            self.existing.as_ref(),
            &mut || {
                if LINKS_EPOCH.with(std::cell::Cell::get) != self.epoch {
                    return Err("document links were reconfigured during save".into());
                }
                Ok(())
            },
        )
    }
    pub(crate) fn note_saved(
        self,
        path: String,
        may_be_open: bool,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.run(Operation::Saved { path, may_be_open }, done);
    }
    pub(crate) fn moved(self, old: String, new: String, done: Box<dyn FnOnce(Result<(), String>)>) {
        self.run(Operation::Moved { old, new }, done);
    }
    fn run(self, operation: Operation, done: Box<dyn FnOnce(Result<(), String>)>) {
        if let Err(error) = self.check() {
            done(Err(error));
            return;
        }
        let captured = LINKS.with(|slot| {
            let mut slot = slot.borrow_mut();
            let links = slot.as_mut()?;
            let path = links.document_links()?.deps().file_path().to_owned();
            let (snapshot, events) = links.capture_saved_snapshot()?;
            Some((path, snapshot, events))
        });
        let Some((path, snapshot, events)) = captured else {
            done(Ok(()));
            return;
        };
        let revision = snapshot.revision();
        let retry_operation = operation.clone();
        let view = self.destination.view;
        let workers = self.services.workers.clone();
        let load_path = path.clone();
        let completion =
            shell_ipc_w::home_files::back_to_core(self.services.core.clone(), move |plan: Plan| {
                let mut state = Some(plan.state);
                let mut outcome = None;
                let admitted = admit(
                    &self.effects,
                    &self.origin,
                    &self.participants,
                    &self.cancel,
                    &self.pending,
                    &self.additional,
                    self.recovery.as_ref(),
                    self.existing.as_ref(),
                    &mut || {
                        if LINKS_EPOCH.with(std::cell::Cell::get) != self.epoch {
                            return Err("document links were replaced".into());
                        }
                        let prepared = state
                            .take()
                            .ok_or("document-link state was admitted twice")?;
                        outcome = Some(LINKS.with(|slot| {
                            slot.borrow_mut()
                                .as_mut()
                                .ok_or_else(|| {
                                    "document-link store is no longer configured".to_owned()
                                })?
                                .apply_saved_snapshot(revision, prepared)
                                .map_err(|_| "revision-conflict".to_owned())
                        }));
                        Ok(())
                    },
                );
                if let Err(error) = admitted {
                    done(Err(error));
                    return;
                }
                match outcome {
                    Some(Err(error)) if error == "revision-conflict" => {
                        self.run(retry_operation, done)
                    }
                    Some(Err(error)) => done(Err(error)),
                    Some(Ok(commits)) => self.persist(path, plan.writes, commits, done),
                    None => done(Err("document-link acknowledgement was not admitted".into())),
                }
            });
        workers.spawn(Box::new(move || {
            let deps = WorkerDeps {
                path: load_path,
                events,
                writes: RefCell::new(Vec::new()),
            };
            let mut model = snapshot.into_model(deps);
            match operation {
                Operation::Saved { path, may_be_open } => {
                    let fresh =
                        !may_be_open || is_freshly_saved_path(&path, now_ms(), &mut stat_sync);
                    if let Err(error) = model.note_tab_path(&mut Contents(view), &path, Some(fresh))
                    {
                        eprintln!("shell: document links: {error:?}");
                    }
                }
                Operation::Moved { old, new } => model.file_moved(&old, &new),
            }
            let state = model.snapshot();
            let writes = std::mem::take(&mut *model.deps().writes.borrow_mut());
            completion(Plan { state, writes });
        }));
    }
    fn persist(
        self,
        path: String,
        writes: Vec<DocumentLinksFile>,
        commits: u64,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let (origin, participants, cancel, pending, additional, effects, recovery, existing) = (
            self.origin.clone(),
            self.participants.clone(),
            self.cancel.clone(),
            self.pending.clone(),
            self.additional.clone(),
            self.effects.clone(),
            self.recovery.clone(),
            self.existing.clone(),
        );
        let completion = shell_ipc_w::home_files::back_to_core(
            self.services.core.clone(),
            move |result: Result<(), String>| {
                if let Err(error) = result {
                    done(Err(error));
                    return;
                }
                if let Err(error) = self.check() {
                    done(Err(error));
                    return;
                }
                let mut kinds = Vec::new();
                LINKS.with(|slot| {
                    if let Some(links) = slot.borrow_mut().as_mut() {
                        links.publish_saved_snapshot_with(commits, &mut |kind| kinds.push(kind));
                    }
                });
                refresh(kinds.into_iter(), self.menu, done);
            },
        );
        let io_path = path.clone();
        metadata::submit(
            &path,
            move || {
                let refusal = std::cell::RefCell::new(None);
                let mut admit = |effect: &mut dyn FnMut()| {
                    admit(
                        &effects,
                        &origin,
                        &participants,
                        &cancel,
                        &pending,
                        &additional,
                        recovery.as_ref(),
                        existing.as_ref(),
                        &mut || {
                            effect();
                            Ok(())
                        },
                    )
                    .map_err(|error| {
                        *refusal.borrow_mut() = Some(error.clone());
                        main_core::node_fs::NodeError {
                            code: None,
                            message: error,
                        }
                    })
                };
                for file in writes {
                    if let Err(error) = main_core::packages::electron_utils::atomic_write::write_json_atomic_authorized(&io_path,&file.to_json(),&mut admit) {
                    if refusal.borrow().is_some() { break; }
                    // DocumentLinks.commit warns but still delivers its change listeners.
                    eprintln!("[document-links] could not write the records: {}",error.message);
                }
                }
                Ok(refusal.into_inner().map_or(Ok(()), Err))
            },
            move |result| {
                completion(
                    result
                        .map_err(|error| error.message)
                        .and_then(|result| result),
                )
            },
        );
    }
}

fn refresh(
    mut kinds: std::vec::IntoIter<TabKind>,
    menu: Option<LinksMenu>,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let Some(kind) = kinds.next() else {
        done(Ok(()));
        return;
    };
    let Some(captured) = menu.clone() else {
        done(Ok(()));
        return;
    };
    let next = Box::new(move |result| match result {
        Ok(()) => refresh(kinds, menu, done),
        Err(error) => done(Err(error)),
    });
    match captured {
        LinksMenu::Interactive(menu) => menu.refresh(kind, next),
        LinksMenu::Recovery(menu) => menu.refresh(kind, next),
        LinksMenu::Existing(menu) => menu.refresh(kind, next),
    }
}
