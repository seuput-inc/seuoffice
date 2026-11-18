//! File preparation and publication run off the core thread; state follows commit.
use super::*;
use main_core::{html::asset_lifecycle as html,markdown::asset_lifecycle as markdown};
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::{atomic_write::atomic_write_file_authorized,save_conflict::{record_disk_state,SaveDiskState}};
use jscompat::node_path::host::{dirname,resolve};

pub(crate) struct Input {
    pub html:bool,pub view:ViewId,pub text:String,pub image_sources:Vec<String>,pub current:Option<String>,pub target:String,pub placed:bool,pub save_as:bool,pub pending:Vec<String>,
}
enum Prepared { Html(html::PreparedSaveAsAssets), Markdown(markdown::PreparedSaveAsAssets) }
pub(crate) struct Committed {input:Input,prepared:Option<Prepared>,sources:Vec<String>,pending:Vec<String>,pub disk:Option<SaveDiskState>}
pub(crate) fn admit(request:&OwnedSave,effect:&mut dyn FnMut()->Result<(),NodeError>)->Result<(),NodeError>{
    let mut result=None;
    request.admit(&mut||{result=Some(effect());Ok(())}).map_err(|message|NodeError{code:None,message})?;
    result.unwrap_or_else(||Err(NodeError{code:None,message:"file effect was not admitted".into()}))
}
impl Committed {
    pub(crate) fn commit(mut input:Input,request:&OwnedSave)->Result<Self,String>{
        request.check_target(std::path::Path::new(&input.target))?;
        let references=if input.html {html::extract_html_asset_references(&input.text).map_err(|e|format!("{e:?}"))?}else{markdown::extract_markdown_image_sources(&input.text).map_err(|e|e.message)?};
        for source in references{if !input.image_sources.contains(&source){input.image_sources.push(source)}}
        let prepared=match &input.current{
            Some(current) if resolve(&[&dirname(current)])!=resolve(&[&dirname(&input.target)])=>{
                Some(if input.html{Prepared::Html(html::prepare_assets_for_save_as_authorized(current,&input.target,&input.text,&input.image_sources,&mut|effect|admit(request,effect)).map_err(|e|e.message)?)}else{Prepared::Markdown(markdown::prepare_assets_for_save_as_authorized(current,&input.target,&input.text,&input.image_sources,&mut|effect|admit(request,effect)).map_err(|e|e.message)?)})
            },_=>None,
        };
        let diagnostics=match &prepared{Some(Prepared::Html(plan))=>plan.staging_diagnostics.as_slice(),Some(Prepared::Markdown(plan))=>plan.staging_diagnostics.as_slice(),None=>&[]};
        if !diagnostics.is_empty(){eprintln!("[{}] asset staging cleanup diagnostics: {diagnostics:?}",if input.html{"html"}else{"markdown"});}
        let (text,sources)=match &prepared{Some(Prepared::Html(p))=>(p.text.as_str(),p.image_sources.clone()),Some(Prepared::Markdown(p))=>(p.text.as_str(),p.image_sources.clone()),None=>(input.text.as_str(),input.image_sources.clone())};
        if !input.placed {
            if let Err(error)=atomic_write_file_authorized(&input.target,text.as_bytes(),&mut|effect|admit(request,&mut||{effect();Ok(())})){
                let rollback=match &prepared{Some(Prepared::Html(p))=>html::rollback_prepared_save_as_assets_authorized(p,&mut|effect|admit(request,effect)),Some(Prepared::Markdown(p))=>markdown::rollback_prepared_save_as_assets_authorized(p,&mut|effect|admit(request,effect)),None=>Ok(())};
                if let Err(error)=rollback{eprintln!("[{}] prepared asset rollback incomplete: {}",if input.html{"html"}else{"markdown"},error.message);}
                return Err(error.message)
            }
        }
        let pending=match &prepared{Some(Prepared::Html(p))=>p.created.iter().map(|r|r.name.clone()).collect(),Some(Prepared::Markdown(p))=>p.created.iter().map(|r|r.name.clone()).collect(),None if input.current.as_ref().is_some_and(|p|resolve(&[p])==resolve(&[&input.target]))=>input.pending.clone(),None=>Vec::new()};
        let disk=record_disk_state(&input.target);
        Ok(Self{input,prepared,sources,pending,disk})
    }
    pub(crate) fn reconcile(self,request:&OwnedSave)->Result<IpcValue,String>{
        let input=&self.input;
        let errors=if input.html{html::reconcile_owned_assets_authorized(&input.target,&self.sources,Some(&self.pending),&mut|effect|admit(request,effect)).map_err(|e|e.message)?.errors}else{markdown::reconcile_owned_assets_authorized(&input.target,&self.sources,Some(&self.pending),&mut|effect|admit(request,effect)).map_err(|e|e.message)?.errors};
        if !errors.is_empty(){eprintln!("[{}] asset reconciliation incomplete: {errors:?}",if input.html{"html"}else{"markdown"});}
        if input.save_as {if let Some(current)=input.current.as_ref().filter(|p|resolve(&[p])!=resolve(&[&input.target])){
            let errors=if input.html{html::resolve_source_pending_after_save_as_authorized(current,&input.pending,&mut|effect|admit(request,effect)).map_err(|e|e.message)?.errors}else{markdown::resolve_source_pending_after_save_as_authorized(current,&input.pending,&mut|effect|admit(request,effect)).map_err(|e|e.message)?.errors};
            if !errors.is_empty(){eprintln!("[{}] source asset reconciliation incomplete: {errors:?}",if input.html{"html"}else{"markdown"});}
        }}
        let (rewrites,text)=match self.prepared{Some(Prepared::Html(p))=>(p.rewrites.into_iter().map(|r|(r.from,r.to)).collect::<Vec<_>>(),None),Some(Prepared::Markdown(p))=>(p.rewrites.into_iter().map(|r|(r.from,r.to)).collect(),Some(p.text)),None=>(Vec::new(),None)};
        if rewrites.is_empty(){return Ok(saved_at(&input.target))}
        let mut entries=vec![("ok".into(),true.into_ipc()),("path".into(),input.target.clone().into_ipc()),("imageRewrites".into(),IpcValue::Array(rewrites.into_iter().map(|(from,to)|obj([("from",from.into_ipc()),("to",to.into_ipc())])).collect()))];
        if let Some(text)=text{entries.push(("writtenText".into(),text.into_ipc()));}
        Ok(IpcValue::Object(entries))
    }
}
