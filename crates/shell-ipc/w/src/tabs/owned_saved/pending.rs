use super::{SavedAuthority,SavedTabAck};
use std::rc::Rc;
use shell_contract::{ViewId,editors::DocumentEditors,ipc::NativeRequestOwner};
use super::super::state::TabsState;

struct Candidate {
    id:String,
    path:String,
    view:Option<ViewId>,
    detached:bool,
    owner:Option<Result<(NativeRequestOwner,Rc<dyn DocumentEditors>),String>>,
}

pub struct PendingDirSave {
    state:Rc<TabsState>,
    authority:SavedAuthority,
    epoch:u64,
    pending:Option<(String,f64)>,
    candidates:Vec<Candidate>,
}

pub struct MovedTabs {
    pub ack:SavedTabAck,
    pub notify:Box<dyn FnOnce(Rc<dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt)->Result<(),String>>,Box<dyn FnOnce(Result<(),String>)>)>,
}

impl PendingDirSave {
    pub(super) fn capture(state:&TabsState,authority:SavedAuthority,ack:&SavedTabAck)->Result<Self,String> {
        let mut candidates = Vec::new();
        for (detached,rows) in [(false,ack.session_strip.get("tabs").and_then(serde_json::Value::as_array).map(Vec::as_slice).unwrap_or(&[])),
            (true,ack.session_detached.as_slice())] {
            for row in rows {
                let Some(id) = row.get("id").and_then(serde_json::Value::as_str) else { continue; };
                let Some(path) = row.get("filePath").and_then(serde_json::Value::as_str).filter(|path| !path.is_empty()) else { continue; };
                let view = ack.session_views.iter().find(|(tab,_)| tab == id).map(|(_,view)| *view);
                let owner = view.map(|view| (|| {
                    let editor = shell_contract::editors::editor_for(&state.provided,view).ok_or("the affected editor is unavailable")?;
                    let native = if view == authority.destination.view { authority.destination.clone() } else {
                        let generation = state.services.views.renderer_generation(view).ok_or("the affected renderer is unavailable")?;
                        authority.effects.capture_owner(view,&generation)?
                    };
                    let owner = editor.capture_document_owner(view,native).map_err(|error| error.to_string())?;
                    Ok((owner,editor))
                })());
                candidates.push(Candidate { id:id.to_owned(),path:path.to_owned(),view,detached,owner });
            }
        }
        authority.check()?;
        let pending = state.pending_dir_for(authority.destination.view);
        Ok(Self { state:state.rc(),epoch:state.manager_epoch.get(),authority,pending,candidates })
    }

    pub fn pending(&self)->Option<&(String,f64)> { self.pending.as_ref() }

    /// Worker input only. Detached canonical matching occurs after the real rename.
    pub fn matching_paths(&self)->Vec<(String,bool)> {
        self.candidates.iter().map(|row| (row.path.clone(),row.detached)).collect()
    }

    pub fn eligible_source(path:&str,root:&str,set_at:f64)->bool {
        super::pending_source_is_eligible(path,root,set_at)
    }

    pub fn select_after_move(old:&str,paths:&[(String,bool)])->Vec<usize> {
        let wanted = main_core::shell::tab_manager::canonical_path(Some(old));
        let mut detached_found = false;
        paths.iter().enumerate().filter_map(|(index,(path,detached))| {
            if *detached {
                if detached_found || main_core::shell::tab_manager::canonical_path(Some(path)) != wanted { return None; }
                detached_found = true;
                Some(index)
            } else { (path == old).then_some(index) }
        }).collect()
    }

    pub fn clear_if_current(&self)->Result<(),String> {
        self.authority.with(|| {
            if self.state.manager_epoch.get() != self.epoch { return Err("the saved tab manager was replaced".into()); }
            let mut state = self.state.inner.borrow_mut();
            let key = i64::from(self.authority.destination.view.0);
            if state.pending_dir_by_wc.get(&key) != self.pending.as_ref() {
                return Err("the pending save folder was replaced".into());
            }
            state.pending_dir_by_wc.remove(&key);
            Ok(())
        })
    }

    pub fn acknowledge_move(self,old:String,new:String,matched:Vec<usize>)->Result<MovedTabs,String> {
        let mut strip_changed = false;
        let mut detached_title = None;
        let mut notifications = Vec::new();
        let mut updates = Vec::new();
        for index in matched {
            let row = self.candidates.get(index).ok_or("the moved tab selection is invalid")?;
            let target = match &row.owner {
                Some(Ok((owner,editor))) => Some((owner.clone(),editor.clone())),
                Some(Err(error)) => return Err(error.clone()),
                None => None,
            };
            if let Some(view) = row.view {
                let details = self.state.details_of(view).ok_or("the affected tab was closed")?;
                if details.tab_id.as_deref() != Some(row.id.as_str()) || details.file_path.as_deref() != Some(row.path.as_str()) {
                    return Err("the affected tab no longer shows its captured file".into());
                }
            }
            let mut authority = self.authority.clone();
            if let Some((owner,_)) = &target {
                authority.destination = owner.clone();
                authority.participants.push(owner.clone());
            }
            authority.with(|| {
                if self.state.manager_epoch.get() != self.epoch { return Err("the saved tab manager was replaced".into()); }
                let mut state = self.state.inner.borrow_mut();
                if row.detached {
                    let view = row.view.ok_or("the affected detached tab has no view")?;
                    let window = state.detached.detached_set_file_record(i64::from(view.0),&new)
                        .ok_or("the affected detached tab was closed")?;
                    detached_title = Some((window,jscompat::node_path::host::basename(&new,None)));
                } else {
                    let manager = state.manager.as_mut().ok_or("the tab manager is unavailable")?;
                    let current = manager.list().into_iter().find(|tab| tab.id == row.id).ok_or("the affected tab was closed")?;
                    if current.file_path.as_deref() != Some(row.path.as_str()) || manager.web_contents_for_tab(&row.id) != row.view {
                        return Err("the affected tab changed during save".into());
                    }
                    if !manager.set_tab_file_by_id(&row.id,&new) { return Err("the affected tab was closed".into()); }
                    strip_changed |= row.view.is_some();
                }
                Ok(())
            })?;
            updates.push(super::SavedSessionUpdate { id:row.id.clone(),view:row.view,path:Some(new.clone()),
                title:jscompat::node_path::host::basename(&new,None) });
            if let (Some(view),Some((owner,editor))) = (row.view,target) { notifications.push((view,owner,editor)); }
        }
        let mut ack = self.state.capture_saved_ack(strip_changed,detached_title,&self.authority,self.epoch)?;
        ack.saved_path = Some(new.clone());
        ack.session_updates = updates;
        let authority = self.authority;
        Ok(MovedTabs { ack,notify:Box::new(move |on_revision,done| notify(notifications.into_iter(),old,new,authority,on_revision,done)) })
    }
}

fn notify(
    mut pending:std::vec::IntoIter<(ViewId,NativeRequestOwner,Rc<dyn DocumentEditors>)>,
    old:String,new:String,authority:SavedAuthority,
    on_revision:Rc<dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt)->Result<(),String>>,
    done:Box<dyn FnOnce(Result<(),String>)>,
) {
    let Some((view,owner,editor)) = pending.next() else { done(Ok(())); return; };
    if authority.existing.is_some() && authority.expected_path.is_none() {
        return done(Err("the original saved destination is unavailable".into()));
    }
    if let (Some(revision), Some(expected_path)) = (&authority.existing, &authority.expected_path) {
        let prior=match revision.snapshot() {
            Ok(snapshot)=>snapshot,
            Err(error)=>return done(Err(error)),
        };
        let mut participants=authority.participants.clone();
        participants.push(authority.origin.clone());
        participants.push(authority.destination.clone());
        participants.push(owner.clone());
        participants.extend(pending.as_slice().iter().map(|(_,owner,_)|owner.clone()));
        let next_old=old.clone();
        let next_new=new.clone();
        let mut next_authority=authority.clone();
        let next_revision=on_revision.clone();
        let completed=Box::new(move |result:Result<Option<shell_contract::ipc::NativeSaveRevisionReceipt>,shell_contract::IpcError>| {
            let receipt=match result { Ok(receipt)=>receipt,Err(error)=>return done(Err(error.to_string())) };
            let mut remaining:Vec<_>=pending.collect();
            if let Some(receipt)=receipt {
                if !receipt.succeeds(&prior) {
                    return done(Err("the saved revision receipt does not follow the captured request".into()));
                }
                let renew=|owner:&mut NativeRequestOwner| -> Result<(),String> {
                    *owner=receipt.renewed_request_owner(owner).ok_or("the saved request owner was not renewed")?.clone();
                    Ok(())
                };
                let replaced=(|| {
                    renew(&mut next_authority.origin)?;
                    renew(&mut next_authority.destination)?;
                    for owner in &mut next_authority.participants { renew(owner)?; }
                    for (_,owner,_) in &mut remaining { renew(owner)?; }
                    let extra:Vec<_>=next_authority.additional.iter().collect();
                    let mut admitted=|| {
                        if next_authority.expected_path.as_deref()!=Some(receipt.snapshot().selection().path()) {
                            return Err("the saved destination changed after publication".into());
                        }
                        Ok(())
                    };
                    next_authority.effects.with_native_save_revision_snapshot_effect(
                        receipt.snapshot(),&next_authority.participants,&next_authority.cancel,
                        &next_authority.pending,&extra,&mut admitted,
                    )?;
                    next_revision(&receipt)
                })();
                if let Err(error)=replaced { return done(Err(error)); }
            }
            notify(remaining.into_iter(),next_old,next_new,next_authority,next_revision,done);
        });
        editor.file_renamed_existing_document(view,std::path::Path::new(&old),std::path::Path::new(&new),
            revision,expected_path,&owner,&participants,&authority.cancel,&authority.pending,&authority.additional,completed);
    } else {
        let next_old=old.clone();
        let next_new=new.clone();
        let next_authority=authority.clone();
        let completed=Box::new(move |result:Result<(),shell_contract::IpcError>| match result {
            Ok(()) => notify(pending,next_old,next_new,next_authority,on_revision,done),
            Err(error) => done(Err(error.to_string())),
        });
        if let Some((receipt,receiver)) = &authority.recovery {
            editor.file_renamed_recovery(view,std::path::Path::new(&old),std::path::Path::new(&new),
                receipt,receiver,&owner,&authority.participants,&authority.cancel,&authority.pending,completed);
        } else {
            editor.file_renamed_many_owned_extra(view,std::path::Path::new(&old),std::path::Path::new(&new),
                &authority.origin,&owner,&authority.participants,&authority.cancel,&authority.pending,&authority.additional,completed);
        }
    }
}
