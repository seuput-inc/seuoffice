//! Recovery file-open continuation, retaining the selected target through every stage.
use shell_contract::{hosts::OwnedEffectsHost, ipc::{NativeRequestOwner,RecoverySeedReceipt}, runtime::{CancelToken,Services}};
use shell_contract::slides::{OwnedRecentMove,OwnedTemplateMove,SlidesRecentFiles,SlidesTemplateSelectionHooks};
use shell_ipc_w::{home_files::{Home,PreparedMovedMetadata},tabs::{PendingDirSave,SavedContinuation,SavedTabAck}};
use crate::host::menu::doc_links::SavedLinks;

#[derive(Clone)]
struct Authority {
    receipt:RecoverySeedReceipt, receiver:NativeRequestOwner, cancel:CancelToken,
    pending:CancelToken, effects:OwnedEffectsHost, services:Services,
}
impl Authority {
    fn admit(&self, effect:&mut dyn FnMut()->Result<(),String>)->Result<(),String> {
        self.effects.with_recovery_open_effect_many(&self.receipt,&self.receiver,&[],&self.cancel,&self.pending,effect)
    }
}
struct Prepared { authority:Authority, metadata:PreparedMovedMetadata, links:SavedLinks, recent:Option<OwnedRecentMove>, templates:Vec<OwnedTemplateMove> }
type Done = Box<dyn FnOnce(Result<(),String>)>;

pub(super) struct WorkerDone<T:Send+'static>(Option<Box<dyn FnOnce(Result<T,String>)+Send>>);
impl<T:Send+'static> WorkerDone<T> {
    pub(super) fn new(done:Box<dyn FnOnce(Result<T,String>)+Send>)->Self { Self(Some(done)) }
    pub(super) fn finish(mut self,result:Result<T,String>) {
        if let Some(done)=self.0.take() { done(result); }
    }
}
impl<T:Send+'static> Drop for WorkerDone<T> {
    fn drop(&mut self) {
        if let Some(done)=self.0.take() { done(Err("The file continuation worker was dropped".into())); }
    }
}

pub(crate) fn docs_file_opened_recovery(
    path:String,receipt:RecoverySeedReceipt,receiver:NativeRequestOwner,cancel:CancelToken,
    pending:CancelToken,effects:OwnedEffectsHost,done:Done,
) {
    let mut done = Some(done);
    let available = crate::host::w::on_core_services_local(|provided,services| {
        let Some(done) = done.take() else { return; };
        let prepared = (|| {
            effects.with_recovery_open_effect_many(&receipt,&receiver,&[],&cancel,&pending,&mut || Ok(()))?;
            let tabs = provided.all::<shell_ipc_w::tabs::TabsState>().into_iter().next().ok_or("tab services unavailable")?;
            let home = provided.all::<Home>().into_iter().next().ok_or("home services unavailable")?;
            let metadata = home.prepare_moved_metadata_recovery(&receipt,&receiver,&cancel,&pending,&effects)?;
            let mut templates=Vec::new();
            let recent=if path.to_ascii_lowercase().ends_with(".pptx") {
                let snapshot=receipt.target().authority_snapshot()?;
                for hooks in provided.all::<dyn SlidesTemplateSelectionHooks>() {
                    templates.push(hooks.prepare_moved_recovery(&receipt,&snapshot,&receiver,&receiver,&[],&cancel,&pending,&effects)?);
                }
                let recent=provided.all::<dyn SlidesRecentFiles>().into_iter().next().ok_or("Slides recent-file service unavailable")?;
                Some(recent.prepare_moved_recovery(&receipt,&snapshot,&receiver,&receiver,&[],&cancel,&pending,&effects)?)
            } else { None };
            let links = SavedLinks::capture_recovery(receipt.clone(),receiver.clone(),Vec::new(),cancel.clone(),pending.clone(),effects.clone(),services.clone())?;
            let authority = Authority { receipt:receipt.clone(),receiver:receiver.clone(),cancel:cancel.clone(),pending:pending.clone(),effects:effects.clone(),services:services.clone() };
            Ok::<_,String>((tabs,Prepared { authority,metadata,links,recent,templates }))
        })();
        let (tabs,prepared) = match prepared { Ok(value)=>value,Err(error)=>return done(Err(error)) };
        let original = path.clone();
        tabs.prepare_docs_file_opened_recovery(path,receipt,receiver,cancel,pending,effects,Box::new(move |result| {
            match result { Ok(stage)=>prepared.apply_pending(original,stage,done),Err(error)=>done(Err(error)) }
        }));
    });
    if !available && let Some(done)=done { done(Err("Recovery file publication requires the core thread".into())); }
}

impl Prepared {
    fn apply_pending(self,path:String,stage:SavedContinuation,done:Done) {
        let Some((dir,set_at))=stage.pending.pending().cloned() else { return finish(stage.ack,stage.publication.session,done); };
        let now=main_core::packages::pdf_structure::files::system_now_ms();
        if now-set_at > 30.0*60.0*1000.0 {
            return match stage.pending.clear_if_current() { Ok(())=>finish(stage.ack,stage.publication.session,done),Err(error)=>done(Err(error)) };
        }
        let authority=self.authority.clone();
        let workers=authority.services.workers.clone();
        let input=path.clone();
        let complete=WorkerDone(Some(shell_ipc_w::home_files::back_to_core(authority.services.core.clone(),move |eligible:Result<bool,String>| match eligible {
            Err(error)=>done(Err(error)),
            Ok(false)=>finish(stage.ack,stage.publication.session,done),
            Ok(true)=>match stage.pending.clear_if_current() {
                Err(error)=>done(Err(error)),
                Ok(())=>self.move_pending(path,dir,stage,done),
            },
        })));
        workers.spawn(Box::new(move || complete.finish(resolve_default_root(&authority).map(|root|PendingDirSave::eligible_source(&input,&root,set_at)))));
    }

    fn move_pending(self,path:String,dir:String,stage:SavedContinuation,done:Done) {
        let paths=stage.pending.matching_paths();
        let authority=self.authority.clone();
        let workers=authority.services.workers.clone();
        let input=path.clone();
        let complete=WorkerDone(Some(shell_ipc_w::home_files::back_to_core(authority.services.core.clone(),move |result:Result<Option<(String,Vec<usize>)>,String>| match result {
            Err(error)=>done(Err(error)),
            Ok(None)=>finish(stage.ack,stage.publication.session,done),
            Ok(Some((new,matched)))=>self.after_move(path,new,matched,stage,done),
        })));
        workers.spawn(Box::new(move || {
            let result=(|| {
                if !std::path::Path::new(&dir).exists() { return Ok(None); }
                let name=jscompat::node_path::host::basename(&input,None);
                let target=main_core::shell::mcp::tools::document_tools::unique_path_in(&dir,&name);
                let mut renamed=None;
                authority.admit(&mut || { renamed=Some(std::fs::rename(&input,&target)); Ok(()) })?;
                match renamed.ok_or("The recovery rename was not admitted")? {
                    Ok(())=>Ok(Some((target,PendingDirSave::select_after_move(&input,&paths)))),
                    Err(error)=>{ eprintln!("[shell] move new file into folder failed: {error}"); Ok(None) }
                }
            })();
            complete.finish(result);
        }));
    }

    fn after_move(self,old:String,new:String,matched:Vec<usize>,stage:SavedContinuation,done:Done) {
        let Self { authority,metadata,links,recent,templates }=self;
        let failed_path=new.clone();
        let done:Done=Box::new(move |result| done(result.map_err(|error|format!("The file moved to {failed_path}, but its file-open continuation failed: {error}"))));
        metadata.replace(old.clone(),new.clone(),Box::new(move |result| {
            if let Err(error)=result { return done(Err(format!("{error:?}"))); }
            let next=authority.clone();
            super::project_file_renamed_recovery(authority.services.clone(),authority.receipt,authority.receiver,Vec::new(),authority.cancel,authority.pending,authority.effects,old.clone(),new.clone(),Box::new(move |result| {
                if let Err(error)=result { return done(Err(error)); }
                let moved_old=old.clone(); let moved_new=new.clone();
                links.moved(old,new,Box::new(move |result| {
                    if let Err(error)=result { return done(Err(error)); }
                    if let Some(recent)=recent {
                        recent(moved_old.clone(),moved_new.clone(),Box::new(|result| {
                            if let Err(error)=result { eprintln!("[slides] moving recent file failed: {error}"); }
                        }));
                    }
                    let core=next.services.core.clone();
                    run_templates(templates.into_iter(),moved_old.clone(),moved_new.clone(),core,Box::new(move |result| {
                        if let Err(error)=result { return done(Err(error)); }
                        let moved=match stage.pending.acknowledge_move(moved_old,moved_new,matched) { Ok(moved)=>moved,Err(error)=>return done(Err(error)) };
                        let ack=moved.ack; let session=stage.publication.session; let publish=stage.publication.publish;
                        publish(ack.clone(),Box::new(move |result| {
                            if let Err(error)=result { return done(Err(error)); }
                            (moved.notify)(Box::new(move |result| match result {
                                Ok(())=>finish(ack,session,done),Err(error)=>done(Err(error)),
                            }));
                        }));
                    }));
                }));
            }));
        }));
    }
}

fn run_templates(
    mut hooks:std::vec::IntoIter<OwnedTemplateMove>,old:String,new:String,
    core:std::sync::Arc<dyn shell_contract::runtime::Thread>,done:Done,
) {
    let Some(hook)=hooks.next() else { return done(Ok(())); };
    let hook_old=old.clone(); let hook_new=new.clone();
    let completed=shell_ipc_w::home_files::back_to_core(core.clone(),move |result| match result {
        Ok(())=>run_templates(hooks,old,new,core,done),Err(error)=>done(Err(error)),
    });
    hook(hook_old,hook_new,completed);
}

fn finish(ack:SavedTabAck,session:Box<dyn FnOnce(SavedTabAck)->Result<shell_contract::hosts::SavedSessionReceipt,String>>,done:Done) {
    match session(ack) { Ok(receipt)=>receipt.finalize(done),Err(error)=>done(Err(error)) }
}

fn resolve_default_root(authority:&Authority)->Result<String,String> {
    use main_core::packages::electron_utils::default_save_dir::configured_default_save_dir_authorized;
    let shared=authority.services.hosts.get::<shell_ipc_w::SharedSaveDirState>().ok_or("save directory state unavailable")?.clone();
    let before=shared.lock().map_err(|error|error.to_string())?.clone();
    let mut resolved=before.clone(); let mut refusal=None;
    let mut admit=|effect:&mut dyn FnMut()|authority.admit(&mut || { effect(); Ok(()) }).map_err(|error| {
        refusal=Some(error.clone()); main_core::node_fs::NodeError { code:None,message:error }
    });
    let root=configured_default_save_dir_authorized(&crate::paths::ShellPaths::new(),&mut resolved,
        main_core::packages::pdf_structure::files::system_now_ms(),&mut admit).map_err(|error|error.0);
    if let Some(error)=refusal { return Err(error); }
    let root=root?; let mut resolved=Some(resolved);
    authority.admit(&mut || {
        if let Ok(mut current)=shared.try_lock() {
            current.publish_resolution(&before,resolved.take().ok_or("save directory cache publication was repeated")?);
        }
        Ok(())
    })?;
    Ok(root)
}
