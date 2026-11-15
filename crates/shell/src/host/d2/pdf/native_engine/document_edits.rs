//! Native document-edit policy and request-scoped PDF object transport.
mod attachments;
mod drawings;
mod forms;
mod markup;
mod pages;
mod outline;
mod stamps;
mod widgets;
pub(crate) mod session;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
use shell_contract::{IpcValue, runtime::CancelToken};

fn field<'a>(value:&'a IpcValue,key:&str)->Option<&'a IpcValue>{match value {IpcValue::Object(entries)=>entries.iter().find(|(name,_)|name==key).map(|(_,value)|value),_=>None}}
fn list(value:Option<&IpcValue>)->&[IpcValue]{match value {Some(IpcValue::Array(values))=>values,_=>&[]}}

/// Apply the document-level phase in one PDF object graph. The caller owns
/// pre-stages, redaction, reprotection and the final atomic-write guard.
pub(super) fn apply_document_edits(bytes:&[u8],request:&IpcValue,modified_at:f64,cancel:&CancelToken)->Result<Vec<u8>,String>{
    if cancel.is_cancelled(){return Err("PDF document session cancelled".into())}
    let mut session=session::Session::open(bytes,cancel.clone())?;
    let count=session.exchange(json!({"op":"pageCount"}))?.as_u64().and_then(|v|usize::try_from(v).ok()).ok_or("invalid PDF page count")?;
    forms::apply_values(&mut session,list(field(request,"formValues")))?;
    pages::rotations(&mut session,request,count)?;
    widgets::apply(&mut session,request,count)?;
    let author=match field(request,"annotationAuthor") {Some(IpcValue::String(value)) if !value.is_empty()=>value.as_str(),_=>"SeuOffice"};
    for markup in list(field(request,"markups")) {markup::apply(&mut session,markup,author,count)?;}
    drawings::apply(&mut session,request,count,author,modified_at)?;
    pages::metadata(&mut session,request,modified_at)?;
    let deleted:Vec<_>=list(field(request,"deletedPages")).iter().filter_map(|value|match value {IpcValue::Number(n)=>Some(*n),_=>None}).collect();
    if let Some(entries)=outline::plan(field(request,"outline"),&deleted,count)? {outline::write(&mut session,&entries)?;}
    pages::finish_pages(&mut session,request,count)?;
    if let Some(attachments)=field(request,"attachments").filter(|value|!matches!(value,IpcValue::Null|IpcValue::Undefined)){attachments::apply(&mut session,attachments)?;}
    pages::appearances(&mut session)?;
    if cancel.is_cancelled(){return Err("PDF document session cancelled".into())}
    let saved=session.exchange(json!({"op":"save"}))?;
    if cancel.is_cancelled(){return Err("PDF document session cancelled".into())}
    let saved=saved.as_str().ok_or("invalid PDF document bytes")?;
    STANDARD.decode(saved).map_err(|error|format!("invalid PDF document bytes: {error}"))
}
