//! Post-save continuations retain the original request and destination document.
use shell_contract::{hosts::OwnedEffectsHost, ipc::NativeRequestOwner, runtime::CancelToken};
mod pending;
pub use pending::{PendingDirSave,MovedTabs};
#[cfg(test)]
mod regressions;

/// Node's stat milliseconds are signed, including dates before the Unix epoch.
fn stat_millis(time: std::io::Result<std::time::SystemTime>) -> f64 {
    time.map(|time| match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64()*1000.0,
        Err(before) => -before.duration().as_secs_f64()*1000.0,
    }).unwrap_or(0.0)
}

pub(super) fn pending_source_is_eligible(path:&str,root:&str,set_at:f64)->bool {
    if !pending_root_matches(path,root) { return false; }
    let Ok(stat) = std::fs::metadata(path) else { return false; };
    let birth = stat_millis(stat.created());
    pending_birth_is_fresh(birth,|| stat_millis(stat.modified()),set_at)
}

fn pending_root_matches(path:&str,root:&str)->bool {
    use jscompat::node_path::host::{dirname,resolve};
    resolve(&[&dirname(path)]) == resolve(&[root])
}

fn pending_birth_is_fresh(birth:f64,modified:impl FnOnce()->f64,set_at:f64)->bool {
    let born = if birth != 0.0 && !birth.is_nan() { birth } else { modified() };
    born >= set_at - 2000.0
}

#[derive(Clone)]
pub(crate) struct SavedAuthority {
    pub(crate) origin: NativeRequestOwner,
    pub(crate) destination: NativeRequestOwner,
    pub(crate) participants:Vec<NativeRequestOwner>,
    pub(crate) cancel: CancelToken,
    pub(crate) pending: CancelToken,
    pub(crate) additional:Vec<CancelToken>,
    pub(crate) recovery:Option<(shell_contract::ipc::RecoverySeedReceipt,NativeRequestOwner)>,
    pub(crate) existing:Option<shell_contract::ipc::NativeSaveRevisionAuthority>,
    pub(crate) saved_document:Option<shell_contract::ipc::NativeSavedDocument>,
    pub(crate) expected_path:Option<std::path::PathBuf>,
    pub(crate) effects: OwnedEffectsHost,
}

impl SavedAuthority {
    pub(crate) fn new(
        origin:NativeRequestOwner,destination:NativeRequestOwner,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,
    )->Self {
        Self::new_extra(origin,destination,participants,cancel,pending,Vec::new(),effects)
    }
    pub(crate) fn new_extra(
        origin:NativeRequestOwner,destination:NativeRequestOwner,mut participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,
    )->Self {
        participants.push(destination.clone());
        Self { origin,destination,participants,cancel,pending,additional,effects,recovery:None,existing:None,saved_document:None,expected_path:None }
    }
    pub(crate) fn recovery(
        receipt:shell_contract::ipc::RecoverySeedReceipt,receiver:NativeRequestOwner,
        cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,
    )->Self {
        Self { origin:receiver.clone(),destination:receiver.clone(),participants:Vec::new(),cancel,pending,additional:Vec::new(),effects,recovery:Some((receipt,receiver)),existing:None,saved_document:None,expected_path:None }
    }
    pub(crate) fn existing(
        destination:shell_contract::ipc::NativeSaveDestination,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,
    )->Self {
        let owner=destination.binding().owner().clone();
        let expected_path=destination.path().to_owned();
        let revision=shell_contract::ipc::NativeSaveRevisionAuthority::new(destination,vec![owner.clone()]);
        Self { origin:owner.clone(),destination:owner,participants,cancel,pending,additional,effects,recovery:None,existing:Some(revision),saved_document:None,expected_path:Some(expected_path) }
    }
    pub(crate) fn existing_revision(
        revision:shell_contract::ipc::NativeSaveRevisionAuthority,expected_path:std::path::PathBuf,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,
    )->Result<Self,String> {
        let snapshot=revision.snapshot()?;
        let owner=snapshot.selection().binding().owner().clone();
        Ok(Self { origin:owner.clone(),destination:owner,participants,cancel,pending,additional,effects,recovery:None,existing:Some(revision),saved_document:None,expected_path:Some(expected_path) })
    }
    pub(crate) fn saved_document(
        document:shell_contract::ipc::NativeSavedDocument,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,
    )->Self {
        let owner=document.owner().clone();
        Self { origin:owner.clone(),destination:owner,participants,cancel,pending,additional,effects,
            recovery:None,existing:None,saved_document:Some(document),expected_path:None }
    }
    pub(crate) fn with<T>(&self, operation: impl FnOnce() -> Result<T,String>) -> Result<T,String> {
        let mut operation = Some(operation);
        let mut value = None;
        let mut invoke=|| {
            let operation = operation.take().ok_or("saved effect was admitted more than once")?;
            value = Some(operation()?);
            Ok(())
        };
        if let Some(document)=&self.saved_document {
            let additional:Vec<_>=self.additional.iter().collect();
            self.effects.with_native_saved_document_input(document,&self.participants,&self.cancel,&self.pending,&additional,&mut |_| invoke())?;
        } else if let Some(revision)=&self.existing {
            let snapshot=revision.snapshot()?;
            let additional:Vec<_>=self.additional.iter().collect();
            self.effects.with_native_save_revision_snapshot_effect(&snapshot,&self.participants,&self.cancel,&self.pending,&additional,&mut || {
                if self.expected_path.as_deref() != Some(snapshot.selection().path()) {
                    return Err("the saved destination changed after publication".into());
                }
                invoke()
            })?;
        } else if let Some((receipt,receiver))=&self.recovery {
            self.effects.with_recovery_open_effect_many(receipt,receiver,&self.participants,&self.cancel,&self.pending,&mut invoke)?;
        } else {
            let additional:Vec<_>=self.additional.iter().collect();
            self.effects.with_owned_effect_many_extra(&self.origin,&self.participants,&self.cancel,&self.pending,&additional,&mut invoke)?;
        }
        value.ok_or_else(|| "saved effect was not admitted".to_owned())
    }

    pub(crate) fn check(&self) -> Result<(),String> { self.with(|| Ok(())) }
}

/// Immutable data produced by a short core save acknowledgement. A native
/// caller enqueues its captured recipients before starting another core stage.
#[derive(Clone)]
pub struct SavedSessionUpdate {
    pub id:String,
    pub view:Option<shell_contract::ViewId>,
    pub path:Option<String>,
    pub title:String,
}

#[derive(Clone)]
pub struct SavedTabAck {
    pub changed: Option<super::host::TabsChanged>,
    pub detached_title: Option<(super::window_snapshot::WindowReservation,String)>,
    pub shell: Option<super::host::ShellWindow>,
    pub session_strip: serde_json::Value,
    pub session_detached: Vec<serde_json::Value>,
    pub session_views: Vec<(String,shell_contract::ViewId)>,
    pub saved_path:Option<String>,
    pub session_updates:Vec<SavedSessionUpdate>,
}

pub struct SavedPublication {
    pub publish:std::rc::Rc<dyn Fn(SavedTabAck,Box<dyn FnOnce(Result<(),String>)>)>,
    pub session:Box<dyn FnOnce(SavedTabAck)->Result<shell_contract::hosts::SavedSessionReceipt,String>>,
}

impl SavedTabAck {
    pub fn event_args(&self)->Option<Vec<shell_contract::IpcValue>> {
        self.changed.as_ref().map(|changed| vec![shell_contract::IpcValue::Array(
            changed.tabs.iter().map(|tab| super::wire::to_ipc(&tab.to_value())).collect()
        )])
    }
}

pub struct SavedContinuation {
    pub ack:SavedTabAck,
    pub publication:SavedPublication,
    pub pending:PendingDirSave,
}

impl super::state::TabsState {
    pub fn set_document_file_owned(
        &self,path:&str,origin:NativeRequestOwner,destination:NativeRequestOwner,
        cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,
    )->Result<SavedTabAck,String> {
        self.acknowledge_saved_file(path,&SavedAuthority::new(origin,destination,Vec::new(),cancel,pending,effects))
    }

    pub fn file_saved_owned(
        &self,path:String,origin:NativeRequestOwner,destination:NativeRequestOwner,
        cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<SavedContinuation,String>)>,
    ) {
        self.file_saved_many_owned(path,origin,destination,Vec::new(),cancel,pending,effects,done);
    }

    pub fn file_saved_many_owned(
        &self,path:String,origin:NativeRequestOwner,destination:NativeRequestOwner,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<SavedContinuation,String>)>,
    ) {
        self.file_saved_many_owned_extra(path,origin,destination,participants,cancel,pending,Vec::new(),effects,done);
    }

    pub fn file_saved_many_owned_extra(
        &self,path:String,origin:NativeRequestOwner,destination:NativeRequestOwner,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<SavedContinuation,String>)>,
    ) {
        let prepared = (|| {
            let authority = SavedAuthority::new_extra(origin.clone(),destination.clone(),participants.clone(),cancel.clone(),pending.clone(),additional.clone(),effects.clone());
            let ack = self.acknowledge_saved_file(&path,&authority)?;
            let publication = self.hooks.prepare_saved_publication_owned(origin.clone(),destination.clone(),authority.participants.clone(),
                cancel.clone(),pending.clone(),additional.clone(),effects.clone())?;
            let pending_move = PendingDirSave::capture(self,authority.clone(),&ack)?;
            let home = self.provided.all::<crate::home_files::Home>().into_iter().next().ok_or("home services unavailable")?;
            let recent = home.prepare_record_recent_file_many_owned_extra(&origin,&destination,&authority.participants,&cancel,&pending,&additional,&effects)?;
            Ok::<_,String>((SavedContinuation { ack,publication,pending:pending_move },recent))
        })();
        let (prepared,recent) = match prepared { Ok(prepared) => prepared,Err(error) => { done(Err(error)); return; } };
        let publish = prepared.publication.publish.clone();
        publish(prepared.ack.clone(),Box::new(move |result| match result {
            Err(error) => done(Err(error)),
            Ok(()) => recent.record(path,Box::new(move |result| done(result.map(|()| prepared)))),
        }));
    }
    pub fn file_saved_existing_document(
        &self,revision:shell_contract::ipc::NativeSaveRevisionAuthority,expected_path:std::path::PathBuf,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<SavedContinuation,String>)>,
    ) {
        let snapshot=match revision.snapshot() { Ok(snapshot)=>snapshot,Err(error)=>return done(Err(error)) };
        let destination=snapshot.selection().clone();
        let path=match expected_path.to_str() { Some(path)=>path.to_owned(),None=>return done(Err("the saved file path is not Unicode".into())) };
        let prepared=(|| {
            let authority=SavedAuthority::existing_revision(revision,expected_path,participants.clone(),cancel.clone(),pending.clone(),additional.clone(),effects.clone())?;
            let ack=self.acknowledge_saved_file(&path,&authority)?;
            let publication=self.hooks.prepare_saved_publication_existing_document(
                destination.clone(),participants.clone(),cancel.clone(),pending.clone(),additional.clone(),effects.clone())?;
            let pending_move=PendingDirSave::capture(self,authority,&ack)?;
            let home=self.provided.all::<crate::home_files::Home>().into_iter().next().ok_or("home services unavailable")?;
            let recent=home.prepare_record_recent_file_existing_document(&destination,&participants,&cancel,&pending,&additional,&effects)?;
            Ok::<_,String>((SavedContinuation { ack,publication,pending:pending_move },recent))
        })();
        let (prepared,recent)=match prepared { Ok(prepared)=>prepared,Err(error)=>return done(Err(error)) };
        let publish=prepared.publication.publish.clone();
        publish(prepared.ack.clone(),Box::new(move |result| match result {
            Err(error)=>done(Err(error)),
            Ok(())=>recent.record(path,Box::new(move |result| done(result.map(|()|prepared)))),
        }));
    }


    pub fn docs_file_saved_owned(
        &self,path:String,origin:NativeRequestOwner,destination:NativeRequestOwner,
        cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<SavedContinuation,String>)>,
    ) {
        self.file_saved_owned(path,origin,destination,cancel,pending,effects,done);
    }

    pub fn prepare_docs_file_opened_recovery(
        &self, path:String, receipt:shell_contract::ipc::RecoverySeedReceipt, receiver:NativeRequestOwner,
        cancel:CancelToken, pending:CancelToken, effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<SavedContinuation,String>)>,
    ) {
        let prepared = (|| {
            let authority = SavedAuthority::recovery(receipt.clone(),receiver.clone(),cancel.clone(),pending.clone(),effects.clone());
            let ack = self.acknowledge_opened_file(&path,&authority)?;
            let publication = self.hooks.prepare_saved_publication_recovery(
                receipt.clone(),receiver.clone(),cancel.clone(),pending.clone(),effects.clone())?;
            let pending_move = PendingDirSave::capture(self,authority,&ack)?;
            let home = self.provided.all::<crate::home_files::Home>().into_iter().next().ok_or("home services unavailable")?;
            let recent = home.prepare_record_recent_file_recovery(&receipt,&receiver,&cancel,&pending,&effects)?;
            Ok::<_,String>((SavedContinuation { ack,publication,pending:pending_move },recent))
        })();
        let (prepared,recent) = match prepared { Ok(prepared) => prepared, Err(error) => return done(Err(error)) };
        let publish = prepared.publication.publish.clone();
        publish(prepared.ack.clone(),Box::new(move |result| match result {
            Err(error) => done(Err(error)),
            Ok(()) => recent.record(path,Box::new(move |result| done(result.map(|()| prepared)))),
        }));
    }

    pub(super) fn publish_opened_saved_document(
        &self, document:shell_contract::ipc::NativeSavedDocument, participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<(),String>)>,
    ) {
        let prepared = (|| {
            let path=document.target().path().path().to_str().ok_or("the saved document path is not Unicode")?.to_owned();
            let details=self.details_of(document.owner().view).ok_or("the loaded document is no longer open")?;
            if details.tab_id.is_none() { return Err("the loaded document has no tab".to_owned()); }
            let authority=SavedAuthority::saved_document(document.clone(),participants.clone(),cancel.clone(),
                pending.clone(),additional.clone(),effects.clone());
            let ack=self.acknowledge_opened_file(&path,&authority)?;
            let publication=self.hooks.prepare_saved_publication_saved_document(document.clone(),participants.clone(),
                cancel.clone(),pending.clone(),additional.clone(),effects.clone())?;
            let home=self.provided.all::<crate::home_files::Home>().into_iter().next().ok_or("home services unavailable")?;
            let recent=home.prepare_record_recent_file_saved_document(&document,&participants,&cancel,&pending,&additional,&effects)?;
            Ok::<_,String>((path,ack,publication,recent))
        })();
        let (path,ack,publication,recent)=match prepared { Ok(value)=>value,Err(error)=>return done(Err(error)) };
        let publish=publication.publish.clone();
        publish(ack.clone(),Box::new(move |result| match result {
            Err(error)=>done(Err(error)),
            Ok(())=>recent.record(path,Box::new(move |result| match result {
                Err(error)=>done(Err(error)),
                Ok(())=>match (publication.session)(ack) {
                    Ok(receipt)=>receipt.finalize(done),
                    Err(error)=>done(Err(error)),
                },
            })),
        }));
    }
}

impl super::state::TabsState {
    pub(super) fn acknowledge_opened_file(&self,path:&str,authority:&SavedAuthority)->Result<SavedTabAck,String> {
        let view = authority.destination.view;
        let tab_id = self.details_of(view).and_then(|details| details.tab_id);
        let epoch = self.manager_epoch.get();
        let (strip_changed,detached_title) = authority.with(|| {
            if self.manager_epoch.get() != epoch { return Err("the tab manager changed during file open".into()); }
            let mut state = self.inner.borrow_mut();
            let changed = match (state.manager.as_mut(),tab_id.as_deref()) {
                (Some(manager),Some(id)) if manager.web_contents_for_tab(id) == Some(view) => manager.set_tab_file_by_id(id,path),
                _ => false,
            };
            let detached = state.detached.detached_set_file_record(i64::from(view.0),path)
                .map(|window| (window,jscompat::node_path::host::basename(path,None)));
            Ok((changed,detached))
        })?;
        let changed = strip_changed || detached_title.is_some();
        let mut ack = self.capture_saved_ack(strip_changed,detached_title,authority,epoch)?;
        ack.saved_path = Some(path.to_owned());
        if changed && let Some(id) = tab_id {
            ack.session_updates.push(SavedSessionUpdate { id,view:Some(view),path:Some(path.to_owned()),
                title:jscompat::node_path::host::basename(path,None) });
        }
        Ok(ack)
    }

    pub(super) fn acknowledge_saved_file(&self,path:&str,authority:&SavedAuthority)->Result<SavedTabAck,String> {
        let view = authority.destination.view;
        let details = self.details_of(view).ok_or("the saved document is no longer open")?;
        let epoch = self.manager_epoch.get();
        let tab_id = details.tab_id;
        let (strip_changed,detached_title) = authority.with(|| {
            if self.manager_epoch.get() != epoch { return Err("the tab manager changed during save".into()); }
            let mut state = self.inner.borrow_mut();
            let strip_changed = match (state.manager.as_mut(),tab_id.as_deref()) {
                (Some(manager),Some(id)) if manager.web_contents_for_tab(id) == Some(view) => manager.set_tab_file_by_id(id,path),
                _ => false,
            };
            let detached = state.detached.detached_set_file_record(i64::from(view.0),path)
                .map(|window| (window,jscompat::node_path::host::basename(path,None)));
            if !strip_changed && detached.is_none() { return Err("the saved document lost its tab".into()); }
            Ok((strip_changed,detached))
        })?;
        let mut ack = self.capture_saved_ack(strip_changed,detached_title,authority,epoch)?;
        ack.saved_path = Some(path.to_owned());
        if let Some(id) = tab_id {
            ack.session_updates.push(SavedSessionUpdate { id,view:Some(view),path:Some(path.to_owned()),
                title:jscompat::node_path::host::basename(path,None) });
        }
        Ok(ack)
    }

    fn capture_saved_ack(
        &self,strip_changed:bool,detached_title:Option<(super::window_snapshot::WindowReservation,String)>,
        authority:&SavedAuthority,epoch:u64,
    )->Result<SavedTabAck,String> {
        let (changed,shell) = {
            let mut state = self.inner.borrow_mut();
            let super::state::Inner { manager,detached,bridge,.. } = &mut *state;
            let changed = if strip_changed {
                manager.as_ref().map(|manager| super::host::TabsChanged {
                    open_paths:manager.open_file_paths(),
                    detached_paths:detached.detached_file_paths(bridge),
                    tabs:manager.list(),
                })
            } else { None };
            (changed,bridge.shell)
        };
        let (session_strip,session_detached) = self.session_snapshot()?;
        let mut session_views = Vec::new();
        for tab in session_strip.get("tabs").and_then(serde_json::Value::as_array).into_iter().flatten()
            .chain(session_detached.iter()) {
            if let Some(id) = tab.get("id").and_then(serde_json::Value::as_str)
                && let Some(view) = self.tab_contents(id) {
                session_views.push((id.to_owned(),view));
            }
        }
        authority.with(|| {
            if self.manager_epoch.get() != epoch { return Err("the tab manager changed before save publication".into()); }
            Ok(())
        })?;
        Ok(SavedTabAck { changed,detached_title,shell,session_strip,session_detached,session_views,saved_path:None,session_updates:Vec::new() })
    }
}

impl super::state::TabsState {
    pub fn set_document_title_owned(
        &self,title:String,origin:NativeRequestOwner,destination:NativeRequestOwner,
        cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<(),String>)>,
    ) {
        self.set_document_title_with_policy(title,origin,destination,Vec::new(),cancel,pending,Vec::new(),effects,false,done);
    }

    pub fn set_slides_document_title_owned(
        &self,title:String,origin:NativeRequestOwner,destination:NativeRequestOwner,
        cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<(),String>)>,
    ) {
        self.set_document_title_with_policy(title,origin,destination,Vec::new(),cancel,pending,Vec::new(),effects,true,done);
    }
    pub fn set_slides_document_title_owned_extra(
        &self,title:String,origin:NativeRequestOwner,destination:NativeRequestOwner,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,
        done:Box<dyn FnOnce(Result<(),String>)>,
    ) {
        self.set_document_title_with_policy(title,origin,destination,participants,cancel,pending,additional,effects,true,done);
    }

    pub fn set_slides_document_title_existing_document(
        &self,title:String,destination:shell_contract::ipc::NativeSaveDestination,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,done:Box<dyn FnOnce(Result<(),String>)>,
    ) {
        let prepared=(|| {
            let authority=SavedAuthority::existing(destination.clone(),participants.clone(),cancel.clone(),pending.clone(),additional.clone(),effects.clone());
            let view=destination.binding().owner().view;
            let details=self.details_of(view).ok_or("the document tab is unavailable")?;
            let epoch=self.manager_epoch.get();
            let update_title=title.clone();
            let (changed,detached_title)=authority.with(|| {
                if self.manager_epoch.get()!=epoch { return Err("the tab manager changed during title update".into()); }
                let mut state=self.inner.borrow_mut();
                let changed=match (state.manager.as_mut(),details.tab_id.as_deref()) {
                    (Some(manager),Some(id)) if manager.web_contents_for_tab(id)==Some(view)=>manager.set_tab_title_by_id(id,&title),
                    _=>false,
                };
                let detached=state.detached.detached_window_record(i64::from(view.0)).map(|window|(window,title));
                Ok((changed,detached))
            })?;
            if !changed && detached_title.is_none() { return Ok(None); }
            let mut ack=self.capture_saved_ack(changed,detached_title,&authority,epoch)?;
            if let Some(id)=details.tab_id {
                ack.session_updates.push(SavedSessionUpdate { id,view:Some(view),path:None,title:update_title });
            }
            let publication=self.hooks.prepare_saved_publication_existing_document(destination,participants,cancel,pending,additional,effects)?;
            Ok::<_,String>(Some((ack,publication)))
        })();
        match prepared {
            Err(error)=>done(Err(error)),
            Ok(None)=>done(Ok(())),
            Ok(Some((ack,publication)))=>{
                let publish=publication.publish.clone();
                publish(ack.clone(),Box::new(move |result| match result {
                    Err(error)=>done(Err(error)),
                    Ok(())=>match (publication.session)(ack) {
                        Ok(receipt)=>receipt.finalize(done),Err(error)=>done(Err(error)),
                    },
                }));
            }
        }
    }


    fn set_document_title_with_policy(
        &self,title:String,origin:NativeRequestOwner,destination:NativeRequestOwner,participants:Vec<NativeRequestOwner>,
        cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,all_detached:bool,
        done:Box<dyn FnOnce(Result<(),String>)>,
    ) {
        let prepared = (|| {
            let authority = SavedAuthority::new_extra(origin.clone(),destination.clone(),participants,cancel.clone(),pending.clone(),additional.clone(),effects.clone());
            let details = self.details_of(destination.view).ok_or("the document tab is unavailable")?;
            let epoch = self.manager_epoch.get();
            let update_title = title.clone();
            let (changed,detached_title) = authority.with(|| {
                if self.manager_epoch.get()!=epoch { return Err("the tab manager changed during title update".into()); }
                let mut state = self.inner.borrow_mut();
                let changed = match (state.manager.as_mut(),details.tab_id.as_deref()) {
                    (Some(manager),Some(id)) if manager.web_contents_for_tab(id) == Some(destination.view) =>
                        manager.set_tab_title_by_id(id,&title),
                    _ => false,
                };
                let detached = if all_detached {
                    state.detached.detached_window_record(i64::from(destination.view.0))
                } else {
                    state.detached.detached_untitled_window(i64::from(destination.view.0))
                }.map(|window| (window,title));
                Ok((changed,detached))
            })?;
            if !changed && detached_title.is_none() { return Ok(None); }
            let mut ack = self.capture_saved_ack(changed,detached_title,&authority,epoch)?;
            if let Some(id) = details.tab_id {
                ack.session_updates.push(SavedSessionUpdate { id,view:Some(destination.view),path:None,title:update_title });
            }
            let publication = self.hooks.prepare_saved_publication_owned(origin,destination,authority.participants.clone(),cancel,pending,additional,effects)?;
            Ok::<_,String>(Some((ack,publication)))
        })();
        match prepared {
            Err(error) => done(Err(error)),
            Ok(None) => done(Ok(())),
            Ok(Some((ack,publication))) => {
                let publish = publication.publish.clone();
                publish(ack.clone(),Box::new(move |result| match result {
                    Err(error) => done(Err(error)),
                    Ok(()) => match (publication.session)(ack) {
                        Ok(receipt) => receipt.finalize(done),
                        Err(error) => done(Err(error)),
                    },
                }));
            }
        }
    }
}
