//! The owned form of index.ts2244–2345 and applyPendingDir1607–1633.
//! Sheets and EPUB skip placement. Slides' recent-file move is intentionally
//! launched without awaiting it, before template selection and editor notices.
//! afterFileMoved is outside the original rename catch, so a metadata/observer
//! error reports the actual moved path and never replays a published prefix.
//! The legacy String-returning W API still warns on that error; native saves
//! consume this Result continuation instead.
use std::path::{Path,PathBuf};
use shell_contract::{hosts::{OwnedEffectsHost,SavedFile,SavedSessionReceipt},ipc::NativeRequestOwner,runtime::{CancelToken,Services}};
use shell_contract::slides::{OwnedRecentMove,OwnedTemplateMove,SlidesRecentFiles,SlidesTemplateSelectionHooks};
use shell_ipc_w::{home_files::{Home,PreparedMovedMetadata},tabs::{PendingDirSave,SavedContinuation,SavedTabAck}};
use main_core::shell::tab_manager::TabKind;
use crate::host::menu::doc_links::SavedLinks;

type Done = Box<dyn FnOnce(Result<SavedFile<PathBuf>,String>)>;
type Session = Box<dyn FnOnce(SavedTabAck)->Result<SavedSessionReceipt,String>>;

#[derive(Clone)]
struct Request {
    origin:NativeRequestOwner,destination:NativeRequestOwner,cancel:CancelToken,pending:CancelToken,
    participants:Vec<NativeRequestOwner>,
    additional:Vec<CancelToken>,
    effects:OwnedEffectsHost,services:Services,
    existing:Option<(shell_contract::ipc::NativeSaveRevisionSnapshot,PathBuf)>,
}
impl Request {
    fn admit(&self,effect:&mut dyn FnMut()->Result<(),String>)->Result<(),String> {
        if let Some((snapshot,path))=&self.existing {
            let extra:Vec<_>=self.additional.iter().collect();
            self.effects.with_native_save_revision_snapshot_effect(snapshot,&self.participants,&self.cancel,&self.pending,&extra,&mut || {
                if snapshot.selection().path()!=path { return Err("the saved destination changed after publication".into()); }
                effect()
            })
        } else {
            let extra:Vec<_>=self.additional.iter().collect();
            self.effects.with_owned_effect_many_extra(&self.origin,&self.participants,&self.cancel,&self.pending,&extra,effect)
        }
    }
    fn project_renamed(self,old:String,new:String,done:Box<dyn FnOnce(Result<(),String>)>) {
        if let Some((snapshot,_))=self.existing {
            crate::host::w_home::project_file_renamed_existing_document(self.services,snapshot.selection().clone(),self.participants,self.cancel,self.pending,self.additional,self.effects,old,new,done);
        } else {
            crate::host::w_home::project_file_renamed_owned_extra(self.services,self.origin,self.destination,self.participants,self.cancel,self.pending,self.additional,self.effects,old,new,done);
        }
    }
}
struct Prepared {
    request:Request,kind:TabKind,links:SavedLinks,metadata:PreparedMovedMetadata,
    recent:Option<OwnedRecentMove>,templates:Vec<OwnedTemplateMove>,
    on_revision:std::rc::Rc<dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt)->Result<(),String>>,
}

/// Core-only entry and completion. No receipt or editor actor crosses a worker.
pub(crate) fn file_saved_owned(
    origin:NativeRequestOwner,destination:NativeRequestOwner,cancel:CancelToken,pending:CancelToken,
    effects:OwnedEffectsHost,path:PathBuf,document_links:bool,done:Done,
) {
    file_saved_many_owned(origin,destination,Vec::new(),cancel,pending,effects,path,document_links,done);
}

pub(crate) fn file_saved_many_owned(
    origin:NativeRequestOwner,destination:NativeRequestOwner,participants:Vec<NativeRequestOwner>,
    cancel:CancelToken,pending:CancelToken,effects:OwnedEffectsHost,path:PathBuf,document_links:bool,done:Done,
) {
    file_saved_many_owned_extra(origin,destination,participants,cancel,pending,Vec::new(),effects,path,document_links,done);
}

pub(crate) fn file_saved_many_owned_extra(
    origin:NativeRequestOwner,destination:NativeRequestOwner,mut participants:Vec<NativeRequestOwner>,
    cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,path:PathBuf,document_links:bool,done:Done,
) {
    participants.push(destination.clone());
    let mut done = Some(done);
    let available = crate::host::w::on_core_services_local(|provided,services| {
        let Some(done) = done.take() else { return; };
        let prepared = (|| {
            let path = path.to_str().ok_or("the saved file path is not Unicode")?.to_owned();
            let extra:Vec<_>=additional.iter().collect();
            effects.with_owned_effect_many_extra(&origin,&participants,&cancel,&pending,&extra,&mut || Ok(()))?;
            let tabs = provided.all::<shell_ipc_w::tabs::TabsState>().into_iter().next().ok_or("tab services unavailable")?;
            let kind = tabs.details_of(destination.view).ok_or("the saved document is no longer open")?.kind;
            let home = provided.all::<Home>().into_iter().next().ok_or("home services unavailable")?;
            let metadata = home.prepare_moved_metadata_many_owned_extra(&origin,&destination,&participants,&cancel,&pending,&additional,&effects)?;
            let links = SavedLinks::capture_many_extra(origin.clone(),destination.clone(),participants.clone(),cancel.clone(),pending.clone(),additional.clone(),effects.clone(),services.clone())?;
            let mut templates = Vec::new();
            let recent = if path.to_ascii_lowercase().ends_with(".pptx") {
                for hooks in provided.all::<dyn SlidesTemplateSelectionHooks>() {
                    templates.push(hooks.prepare_moved_many_owned_extra(&origin,&destination,&participants,&cancel,&pending,&additional,&effects)?);
                }
                let recent = provided.all::<dyn SlidesRecentFiles>().into_iter().next().ok_or("Slides recent-file service unavailable")?;
                Some(recent.prepare_moved_many_owned_extra(&origin,&destination,&participants,&cancel,&pending,&additional,&effects)?)
            } else { None };
            let request = Request { origin,destination,participants,cancel,pending,additional,effects,services:services.clone(),existing:None };
            let on_revision = std::rc::Rc::new(|_:&shell_contract::ipc::NativeSaveRevisionReceipt| Err("renderer saved-file publication cannot accept a native revision".into()));
            Ok::<_,String>((tabs,path,Prepared { request,kind,links,metadata,recent,templates,on_revision }))
        })();
        let (tabs,path,prepared) = match prepared { Ok(value) => value,Err(error) => { done(Err(error)); return; } };
        let request = prepared.request.clone();
        let saved_path = path.clone();
        tabs.file_saved_many_owned_extra(path,request.origin,request.destination,request.participants,request.cancel,request.pending,request.additional,request.effects,
            Box::new(move |result| {
                let stage = match result { Ok(stage) => stage,Err(error) => { done(Err(error)); return; } };
                if document_links {
                    let links = prepared.links.clone();
                    let may_be_open = prepared.kind == TabKind::Slides;
                    let path = saved_path.clone();
                    links.note_saved(saved_path,may_be_open,Box::new(move |result| match result {
                        Ok(()) => prepared.apply_pending(path,stage,done),Err(error) => done(Err(error)),
                    }));
                } else { prepared.apply_pending(saved_path,stage,done); }
            }));
    });
    if !available && let Some(done) = done { done(Err("owned saved-file publication requires the registered core thread".into())); }
}
pub(crate) fn file_saved_existing_document(
    revision:shell_contract::ipc::NativeSaveRevisionAuthority,expected_path:PathBuf,mut participants:Vec<NativeRequestOwner>,
    cancel:CancelToken,pending:CancelToken,additional:Vec<CancelToken>,effects:OwnedEffectsHost,document_links:bool,
    on_revision:std::rc::Rc<dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt)->Result<(),String>>,done:Done,
) {
    let snapshot=match revision.snapshot() { Ok(snapshot)=>snapshot,Err(error)=>return done(Err(error)) };
    let destination=snapshot.selection().clone();
    let owner=destination.binding().owner().clone();
    participants.push(owner.clone());
    let mut done=Some(done);
    let available=crate::host::w::on_core_services_local(|provided,services| {
        let Some(done)=done.take() else { return; };
        let prepared=(|| {
            let path=expected_path.to_str().ok_or("the saved file path is not Unicode")?.to_owned();
            let extra:Vec<_>=additional.iter().collect();
            effects.with_native_save_revision_snapshot_effect(&snapshot,&participants,&cancel,&pending,&extra,&mut || {
                if snapshot.selection().path()!=expected_path { return Err("the saved destination changed after publication".into()); }
                Ok(())
            })?;
            let tabs=provided.all::<shell_ipc_w::tabs::TabsState>().into_iter().next().ok_or("tab services unavailable")?;
            let kind=tabs.details_of(owner.view).ok_or("the saved document is no longer open")?.kind;
            let home=provided.all::<Home>().into_iter().next().ok_or("home services unavailable")?;
            let metadata=home.prepare_moved_metadata_existing_document(&destination,&participants,&cancel,&pending,&additional,&effects)?;
            let links=SavedLinks::capture_existing_document(destination.clone(),participants.clone(),cancel.clone(),pending.clone(),additional.clone(),effects.clone(),services.clone())?;
            let mut templates=Vec::new();
            let recent=if path.to_ascii_lowercase().ends_with(".pptx") {
                for hooks in provided.all::<dyn SlidesTemplateSelectionHooks>() {
                    templates.push(hooks.prepare_moved_existing_document(destination.binding(),&participants,&cancel,&pending,&additional,&effects)?);
                }
                let recent=provided.all::<dyn SlidesRecentFiles>().into_iter().next().ok_or("Slides recent-file service unavailable")?;
                Some(recent.prepare_moved_existing_document(destination.binding(),&participants,&cancel,&pending,&additional,&effects)?)
            } else { None };
            let request=Request { origin:owner.clone(),destination:owner,participants,cancel,pending,additional,effects,
                services:services.clone(),existing:Some((snapshot.clone(),expected_path.clone())) };
            Ok::<_,String>((tabs,path,Prepared { request,kind,links,metadata,recent,templates,on_revision }))
        })();
        let (tabs,path,prepared)=match prepared { Ok(value)=>value,Err(error)=>return done(Err(error)) };
        let request=prepared.request.clone();
        let saved_path=path.clone();
        tabs.file_saved_existing_document(revision,expected_path,request.participants,request.cancel,request.pending,request.additional,request.effects,
            Box::new(move |result| {
                let stage=match result { Ok(stage)=>stage,Err(error)=>return done(Err(error)) };
                if document_links {
                    let links=prepared.links.clone(); let may_be_open=prepared.kind==TabKind::Slides;
                    let path=saved_path.clone();
                    links.note_saved(saved_path,may_be_open,Box::new(move |result| match result {
                        Ok(())=>prepared.apply_pending(path,stage,done),Err(error)=>done(Err(error)),
                    }));
                } else { prepared.apply_pending(saved_path,stage,done); }
            }));
    });
    if !available && let Some(done)=done { done(Err("existing-document saved-file publication requires the registered core thread".into())); }
}


impl Prepared {
    fn apply_pending(self,path:String,stage:SavedContinuation,done:Done) {
        if matches!(self.kind,TabKind::Sheets|TabKind::Epub) {
            finish(path,stage.ack,stage.publication.session,done); return;
        }
        let Some((dir,set_at)) = stage.pending.pending().cloned() else {
            finish(path,stage.ack,stage.publication.session,done); return;
        };
        let now = main_core::packages::pdf_structure::files::system_now_ms();
        if now - set_at > 30.0*60.0*1000.0 {
            match stage.pending.clear_if_current() {
                Ok(()) => finish(path,stage.ack,stage.publication.session,done),Err(error) => done(Err(error)),
            }
            return;
        }
        let request = self.request.clone();
        let workers = request.services.workers.clone();
        let eligibility_path = path.clone();
        let completion = shell_ipc_w::home_files::back_to_core(request.services.core.clone(),move |eligible:Result<bool,String>| {
            match eligible {
                Err(error) => done(Err(error)),
                Ok(false) => finish(path,stage.ack,stage.publication.session,done),
                Ok(true) => {
                    if let Err(error) = stage.pending.clear_if_current() { done(Err(error)); return; }
                    self.move_pending(path,dir,stage,done);
                }
            }
        });
        workers.spawn(Box::new(move || {
            let result = resolve_default_root(&request).map(|root| PendingDirSave::eligible_source(&eligibility_path,&root,set_at));
            completion(result);
        }));
    }

    fn move_pending(self,path:String,dir:String,stage:SavedContinuation,done:Done) {
        let paths = stage.pending.matching_paths();
        let request = self.request.clone();
        let workers = request.services.workers.clone();
        let worker_path = path.clone();
        let completion = shell_ipc_w::home_files::back_to_core(request.services.core.clone(),move |moved:Result<Option<(String,Vec<usize>)>,String>| {
            match moved {
                Err(error) => done(Err(error)),
                Ok(None) => finish(path,stage.ack,stage.publication.session,done),
                Ok(Some((new,matched))) => self.after_move(path,new,matched,stage,done),
            }
        });
        workers.spawn(Box::new(move || {
            let run = || -> Result<Option<(String,Vec<usize>)>,String> {
                if !Path::new(&dir).exists() { return Ok(None); }
                let name = jscompat::node_path::host::basename(&worker_path,None);
                let target = main_core::shell::mcp::tools::document_tools::unique_path_in(&dir,&name);
                let mut renamed = None;
                request.admit(&mut || { renamed = Some(std::fs::rename(&worker_path,&target)); Ok(()) })?;
                match renamed.ok_or("the pending save rename was not admitted")? {
                    Err(error) => { eprintln!("[shell] move new file into folder failed: {error}"); Ok(None) }
                    Ok(()) => Ok(Some((target,PendingDirSave::select_after_move(&worker_path,&paths)))),
                }
            };
            completion(run());
        }));
    }

    fn after_move(self,old:String,new:String,matched:Vec<usize>,stage:SavedContinuation,done:Done) {
        let Self { request,links,metadata,recent,templates,on_revision,.. } = self;
        let failure_path = new.clone();
        let done:Done = Box::new(move |result| done(result.map_err(|error| format!("The file moved to {failure_path}, but its saved-file continuation failed: {error}"))));
        let metadata_old = old.clone();
        let metadata_new = new.clone();
        metadata.replace(metadata_old,metadata_new,Box::new(move |result| {
            if let Err(error) = result { done(Err(format!("{error:?}"))); return; }
            let next_request = request.clone();
            request.project_renamed(old.clone(),new.clone(),Box::new(move |result| {
                    if let Err(error) = result { done(Err(error)); return; }
                    let moved_old = old.clone();
                    let moved_new = new.clone();
                    links.moved(old,new,Box::new(move |result| {
                        if let Err(error) = result { done(Err(error)); return; }
                        // The original explicitly launches this promise without awaiting it.
                        if let Some(recent) = recent {
                            recent(moved_old.clone(),moved_new.clone(),Box::new(|result| {
                                if let Err(error) = result { eprintln!("[slides] moving recent file failed: {error}"); }
                            }));
                        }
                        let core = next_request.services.core.clone();
                        run_templates(templates.into_iter(),moved_old.clone(),moved_new.clone(),core,Box::new(move |result| {
                            if let Err(error) = result { done(Err(error)); return; }
                            let moved = match stage.pending.acknowledge_move(moved_old,moved_new.clone(),matched) {
                                Ok(moved) => moved,Err(error) => { done(Err(error)); return; }
                            };
                            let ack = moved.ack;
                            let session = stage.publication.session;
                            let publish = stage.publication.publish;
                            publish(ack.clone(),Box::new(move |result| {
                                if let Err(error) = result { done(Err(error)); return; }
                                (moved.notify)(on_revision,Box::new(move |result| match result {
                                    Ok(()) => finish(moved_new,ack,session,done),Err(error) => done(Err(error)),
                                }));
                            }));
                        }));
                    }));
                }));
        }));
    }
}

fn finish(path:String,ack:SavedTabAck,session:Session,done:Done) {
    done(session(ack).map(|session| SavedFile { path:PathBuf::from(path),session }));
}

fn run_templates(
    mut hooks:std::vec::IntoIter<OwnedTemplateMove>,old:String,new:String,
    core:std::sync::Arc<dyn shell_contract::runtime::Thread>,done:Box<dyn FnOnce(Result<(),String>)>,
) {
    let Some(hook) = hooks.next() else { done(Ok(())); return; };
    let hook_old = old.clone();
    let hook_new = new.clone();
    let completion = shell_ipc_w::home_files::back_to_core(core.clone(),move |result| match result {
        Ok(()) => run_templates(hooks,old,new,core,done),Err(error) => done(Err(error)),
    });
    hook(hook_old,hook_new,completion);
}

fn resolve_default_root(request:&Request)->Result<String,String> {
    use main_core::packages::electron_utils::default_save_dir::configured_default_save_dir_authorized;
    let shared = request.services.hosts.get::<shell_ipc_w::SharedSaveDirState>().ok_or("save directory state unavailable")?.clone();
    let before = shared.lock().map_err(|error| error.to_string())?.clone();
    let mut resolved = before.clone();
    let mut refusal = None;
    let mut admit = |effect:&mut dyn FnMut()| request.admit(&mut || { effect(); Ok(()) }).map_err(|error| {
        refusal = Some(error.clone());
        main_core::node_fs::NodeError { code:None,message:error }
    });
    let root = configured_default_save_dir_authorized(&crate::paths::ShellPaths::new(),&mut resolved,
        main_core::packages::pdf_structure::files::system_now_ms(),&mut admit).map_err(|error| error.0);
    if let Some(error) = refusal { return Err(error); }
    let root = root?;
    let mut resolved = Some(resolved);
    request.admit(&mut || {
        if let Ok(mut current) = shared.try_lock() {
            let resolved = resolved.take().ok_or("save directory cache publication was repeated")?;
            current.publish_resolution(&before,resolved);
        }
        Ok(())
    })?;
    Ok(root)
}
