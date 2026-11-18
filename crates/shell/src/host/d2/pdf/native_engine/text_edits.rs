//! PDF editor text content and final-output verification.
use std::{collections::HashMap,sync::LazyLock};
use serde_json::{json,Value};
use shell_contract::{IpcValue,runtime::CancelToken};
use super::super::pdfium_session::PdfiumSession;

mod execution;
mod font;
mod inserts;
mod matching;
mod rebuild;
mod validation;

pub(super) fn apply_text_edits(bytes:&[u8],edits:&[IpcValue],cancel:&CancelToken)->Result<(Vec<u8>,Vec<IpcValue>),String>{let (bytes,mut skipped)=execution::apply(bytes,edits,cancel,super::super::pdf_codec::relabel_opentype_font_files)?;for item in &mut skipped{if let IpcValue::Object(fields)=item{fields.retain(|(key,_)|key!="__previewProbe")}}Ok((bytes,skipped))}
pub(super) fn apply_text_inserts(bytes:&[u8],edits:&[IpcValue],cancel:&CancelToken)->Result<(Vec<u8>,Vec<IpcValue>),String>{inserts::apply(bytes,edits,cancel,super::super::pdf_codec::relabel_opentype_font_files)}
pub(super) fn validate_text_edits(bytes:&[u8],edits:&[IpcValue],cancel:&CancelToken)->Result<IpcValue,String>{validation::validate(bytes,edits,cancel)}
pub(super) fn can_draw_text(text:&str,font:Option<&str>,bold:bool,italic:bool)->Result<bool,String>{Ok(validation::can_draw(text,font,bold,italic))}
pub(super) fn erase_text_runs(session:&mut PdfiumSession,doc:u64,page:u64,page_index:f64,bytes:&[u8],probes:&[IpcValue],cancel:&CancelToken)->Result<Vec<bool>,String>{let index=if page_index.is_finite()&&page_index>=0.0&&page_index.fract()==0.0&&page_index<=u32::MAX as f64{page_index as usize}else{usize::MAX};execution::erase(session,doc,page,index,bytes,probes,cancel)}

fn reference(id:u64)->Value{json!({"ref":id})}
fn offset(id:u64,n:usize)->Value{json!({"ref":id,"offset":n})}
fn call(session:&mut PdfiumSession,name:&str,args:&[Value])->Result<Value,String>{session.call(name,args).map_err(|e|e.to_string())}
fn integer(value:Value)->Result<i64,String>{value.as_i64().ok_or_else(||"invalid PDFium integer result".into())}
fn handle(value:Value)->Result<Option<u64>,String>{if value==json!(0){return Ok(None)}value.get("ref").and_then(Value::as_u64).map(Some).ok_or_else(||"invalid PDFium object handle".into())}
fn buffer<T>(session:&mut PdfiumSession,len:usize,f:impl FnOnce(&mut PdfiumSession,u64)->Result<T,String>)->Result<T,String>{let id=session.alloc(len).map_err(|e|e.to_string())?;let result=f(session,id);let free=session.free(id).map_err(|e|e.to_string());match result{Err(e)=>Err(e),Ok(value)=>{free?;Ok(value)}}}
fn field<'a>(value:&'a IpcValue,name:&str)->Option<&'a IpcValue>{match value{IpcValue::Object(fields)=>fields.iter().find(|(k,_)|k==name).map(|(_,v)|v),_=>None}}
fn number(value:Option<&IpcValue>)->Option<f64>{match value{Some(IpcValue::Number(n))=>Some(*n),_=>None}}
fn text(value:Option<&IpcValue>)->Option<&str>{match value{Some(IpcValue::String(s))=>Some(s),_=>None}}
fn page_index(value:Option<&IpcValue>)->Option<usize>{let n=number(value)?;if n.is_finite()&&n>=0.0&&n.fract()==0.0&&n<=u32::MAX as f64{Some(n as usize)}else{None}}
fn skip(page:IpcValue,reason:String)->IpcValue{IpcValue::Object(vec![("pageIndex".into(),page),("reason".into(),IpcValue::String(reason))])}

static RADICALS:LazyLock<HashMap<char,char>>=LazyLock::new(||{
 let source=include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/../../apps/pdf/src/shared/radicals.ts"));
 fn code(s:&str)->Option<char>{let s=s.trim().strip_prefix("'\\u")?.split('\'').next()?;let s=s.strip_prefix('{').and_then(|s|s.strip_suffix('}')).unwrap_or(s);char::from_u32(u32::from_str_radix(s,16).ok()?)}
 source.lines().filter_map(|line|{let (a,b)=line.split_once(':')?;Some((code(a)?,code(b)?))}).collect()
});
fn norm(text:&str)->String{let folded=text.chars().map(|c|RADICALS.get(&c).copied().unwrap_or(c)).collect::<String>();main_core::pdf::font_locate::nfkc(&folded).chars().filter(|c|!jscompat::is_js_space(*c)).collect()}
#[derive(Clone,Debug)]
struct TextObject {handle:u64,index:usize,font:Option<u64>,text:String,bounds:[f64;4],chain:Vec<u64>,ordinals:Vec<usize>}
type Matrix=[f64;6];
const ID:Matrix=[1.0,0.0,0.0,1.0,0.0,0.0];
fn concat(a:Matrix,b:Matrix)->Matrix{[a[0]*b[0]+a[1]*b[2],a[0]*b[1]+a[1]*b[3],a[2]*b[0]+a[3]*b[2],a[2]*b[1]+a[3]*b[3],a[4]*b[0]+a[5]*b[2]+b[4],a[4]*b[1]+a[5]*b[3]+b[5]]}
fn transformed(r:[f64;4],m:Matrix)->[f64;4]{let mut out=[f64::INFINITY,f64::INFINITY,f64::NEG_INFINITY,f64::NEG_INFINITY];for (x,y) in [(r[0],r[1]),(r[2],r[1]),(r[0],r[3]),(r[2],r[3])]{let x1=m[0]*x+m[2]*y+m[4];let y1=m[1]*x+m[3]*y+m[5];out=[out[0].min(x1),out[1].min(y1),out[2].max(x1),out[3].max(y1)];}out}
fn floats(session:&mut PdfiumSession,id:u64,n:usize)->Result<Vec<f64>,String>{let bytes=session.read(id,0,n*4).map_err(|e|e.to_string())?;Ok(bytes.chunks_exact(4).map(|v|f64::from(f32::from_le_bytes([v[0],v[1],v[2],v[3]]))).collect())}
fn matrix(session:&mut PdfiumSession,obj:u64)->Result<Matrix,String>{buffer(session,24,|s,b|{if integer(call(s,"FPDFPageObj_GetMatrix",&[reference(obj),reference(b)])?)?==0{return Ok(ID)}let v=floats(s,b,6)?;Ok([v[0],v[1],v[2],v[3],v[4],v[5]])})}
fn object_text(session:&mut PdfiumSession,obj:u64,text_page:u64,to_page:Matrix,index:usize,chain:&[u64],ordinals:&[usize])->Result<Option<TextObject>,String>{let len=integer(call(session,"FPDFTextObj_GetText",&[reference(obj),reference(text_page),json!(0),json!(0)])?)?;let len=usize::try_from(len).map_err(|_|"invalid PDF text length")?;let text=if len>2{buffer(session,len,|s,b|{call(s,"FPDFTextObj_GetText",&[reference(obj),reference(text_page),reference(b),json!(len)])?;let data=s.read(b,0,len-2).map_err(|e|e.to_string())?;let units=data.chunks_exact(2).map(|p|u16::from_le_bytes([p[0],p[1]])).collect::<Vec<_>>();Ok(String::from_utf16_lossy(&units))})?}else{String::new()};let font=handle(call(session,"FPDFTextObj_GetFont",&[reference(obj)])?)?;buffer(session,16,|s,b|{if integer(call(s,"FPDFPageObj_GetBounds",&[reference(obj),reference(b),offset(b,4),offset(b,8),offset(b,12)])?)?==0{return Ok(None)}let v=floats(s,b,4)?;Ok(Some(TextObject{handle:obj,index,font,text,bounds:transformed([v[0],v[1],v[2],v[3]],to_page),chain:chain.to_vec(),ordinals:ordinals.to_vec()}))})}
fn walk(session:&mut PdfiumSession,parent:u64,text_page:u64,is_form:bool,to_page:Matrix,chain:&mut Vec<u64>,ordinals:&mut Vec<usize>,top:usize,out:&mut Vec<TextObject>)->Result<(),String>{if chain.len()>8{return Ok(())}let (count,get)=if is_form{("FPDFFormObj_CountObjects","FPDFFormObj_GetObject")}else{("FPDFPage_CountObjects","FPDFPage_GetObject")};let n=integer(call(session,count,&[reference(parent)])?)?;let mut ordinal=0;for i in 0..n {let Some(obj)=handle(call(session,get,&[reference(parent),json!(i)])?)? else{continue};let kind=integer(call(session,"FPDFPageObj_GetType",&[reference(obj)])?)?;let index=if is_form{top}else{usize::try_from(i).map_err(|_|"invalid page object index")?};if kind==1{if let Some(obj)=object_text(session,obj,text_page,to_page,index,chain,ordinals)?{out.push(obj)}}else if kind==5{let transform=concat(matrix(session,obj)?,to_page);chain.push(obj);ordinals.push(ordinal);walk(session,obj,text_page,true,transform,chain,ordinals,index,out)?;chain.pop();ordinals.pop();ordinal+=1}}Ok(())}
fn ordered(objects:&[TextObject])->Vec<&TextObject>{let mut all=objects.iter().collect::<Vec<_>>();all.sort_by(|a,b|b.bounds[3].total_cmp(&a.bounds[3]));let mut rows:Vec<Vec<&TextObject>>=Vec::new();for o in all{let same=rows.last().and_then(|r|r.first()).is_some_and(|first|{let overlap=first.bounds[3].min(o.bounds[3])-first.bounds[1].max(o.bounds[1]);overlap>=0.5*(first.bounds[3]-first.bounds[1]).min(o.bounds[3]-o.bounds[1])});if !same{rows.push(Vec::new())}if let Some(row)=rows.last_mut(){row.push(o)}}for row in &mut rows{row.sort_by(|a,b|a.bounds[0].total_cmp(&b.bounds[0]))}rows.into_iter().flatten().collect()}

pub(super) fn verify_text_edits(bytes:&[u8],edits:&[IpcValue],password:Option<&str>,cancel:&CancelToken)->Result<Vec<IpcValue>,String>{let mut session=PdfiumSession::open_with_cancel(bytes,password,cancel.clone()).map_err(|e|e.to_string())?;let doc=session.document();let count=integer(call(&mut session,"FPDF_GetPageCount",&[reference(doc)])?)?;let mut failures=Vec::new();let mut pages:Vec<(usize,Vec<&IpcValue>)>=Vec::new();for edit in edits{let original=field(edit,"pageIndex").cloned().unwrap_or(IpcValue::Undefined);let Some(page)=page_index(Some(&original)).filter(|p|i64::try_from(*p).is_ok_and(|p|p<count)) else{failures.push(skip(original,"page missing from saved output".into()));continue};if let Some((_,group))=pages.iter_mut().find(|(p,_)|*p==page){group.push(edit)}else{pages.push((page,vec![edit]))}}
 for (index,group) in pages{if cancel.is_cancelled(){return Err("cancelled".into())}let Some(page)=handle(call(&mut session,"FPDF_LoadPage",&[reference(doc),json!(index)])?)? else{for _ in group{failures.push(skip(IpcValue::Number(index as f64),"page unreadable in saved output".into()))}continue};let result=(||{let text_page=handle(call(&mut session,"FPDFText_LoadPage",&[reference(page)])?)?.ok_or("could not load PDF text page")?;let result=(||{let mut objects=Vec::new();walk(&mut session,page,text_page,false,ID,&mut Vec::new(),&mut Vec::new(),0,&mut objects)?;let stream=norm(&objects.iter().map(|o|o.text.as_str()).collect::<String>());let visual=norm(&ordered(&objects).iter().map(|o|o.text.as_str()).collect::<String>());for edit in group{let replacement=text(field(edit,"newText")).unwrap_or("");for line in replacement.split('\n'){let line=norm(line);if !line.is_empty()&&!stream.contains(&line)&&!visual.contains(&line){let snippet=line.encode_utf16().take(20).collect::<Vec<_>>();failures.push(skip(IpcValue::Number(index as f64),format!("replacement text missing from saved output (\"{}\")",String::from_utf16_lossy(&snippet))));break}}}Ok(())})();let closed=call(&mut session,"FPDFText_ClosePage",&[reference(text_page)]);result?;closed?;Ok::<(),String>(())})();let closed=call(&mut session,"FPDF_ClosePage",&[reference(page)]);result?;closed?;}Ok(failures)}

#[cfg(test)]
mod tests {use super::*;#[test]fn compatibility_radicals_and_whitespace_match(){assert_eq!(norm("⺅ Ａ\n"),norm("亻A"));}#[test]fn geometry_order_tracks_visual_rows(){let make=|text:&str,bounds|TextObject{handle:0,index:0,font:None,text:text.into(),bounds,chain:vec![],ordinals:vec![]};let objs=vec![make("B",[20.0,0.0,30.0,10.0]),make("A",[0.0,0.0,10.0,10.0])];assert_eq!(ordered(&objs).iter().map(|o|o.text.as_str()).collect::<String>(),"AB");}}
