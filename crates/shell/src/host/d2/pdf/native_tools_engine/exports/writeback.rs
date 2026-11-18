//! Native orchestration for translated PDF writeback.
use std::{collections::{HashMap,HashSet},path::Path};
use base64::{Engine as _,engine::general_purpose::STANDARD};
use serde_json::json;
use shell_contract::{IpcValue as V,runtime::CancelToken};
use shell_ipc_d2::pdf::tools::JobFailure;
use super::translation;
use super::super::super::{job_source::Source,pdfium_session::PdfiumSession,native_callbacks};
mod analyze;
mod bitmap;
mod font;
mod frame;
mod layout;
mod model;
mod pdfium;
mod place;
mod read;

fn obj(fields:Vec<(&str,V)>)->V {V::Object(fields.into_iter().map(|(k,v)|(k.into(),v)).collect())}
fn string(s:impl Into<String>)->V {V::String(s.into())}
fn failure(message:impl AsRef<str>,cancel:&CancelToken)->JobFailure {if cancel.is_cancelled(){JobFailure::new("cancelled",None)}else{JobFailure::new("failed",Some(message.as_ref()))}}
fn active(cancel:&CancelToken)->Result<(),JobFailure>{if cancel.is_cancelled(){Err(JobFailure::new("cancelled",None))}else{Ok(())}}
fn info(rec:&model::UnitRec,reason:Option<&str>)->V {let mut fields=vec![("unitKey",string(&rec.key)),("blockId",string(&rec.block_id)),("page",V::Number(rec.page as f64))];if let Some(reason)=reason{fields.push(("reason",string(reason)))}obj(fields)}
fn complex(text:&str)->bool{text.chars().any(|c|matches!(c as u32,0x0590..=0x08ff|0x0900..=0x0dff|0x0f00..=0x0fff|0x1000..=0x109f|0x1780..=0x17ff|0xfb1d..=0xfdff|0xfe70..=0xfeff))}
struct Writable<'a>{rec:&'a model::UnitRec,parts:Vec<read::Part>,text:String,face:Option<usize>}
fn line_height(parts:&[read::Part],size:f64)->f64 {let gaps=parts.iter().flat_map(|p|p.lines.windows(2).map(|w|w[0]-w[1])).collect::<Vec<_>>();if gaps.is_empty()||size<=0.0{1.2}else{(analyze::median(&gaps)/size).clamp(1.0,2.5)}}
fn align(rec:&model::UnitRec,parts:&[read::Part],model:&model::Model)->layout::Align {if rec.figure.is_some(){return layout::Align::Center}let p=&parts[0];if !rec.title_like&&!(parts.len()==1&&p.lines.len()<=1){return layout::Align::Left}let width=model.pages.iter().find(|v|v.page==p.page).map(|v|v.width).unwrap_or(612.0);if (analyze::center(p.bounds).0-width/2.0).abs()<width*0.02&&p.bounds.x1-p.bounds.x0<width*0.8{layout::Align::Center}else{layout::Align::Left}}
fn subset(font:&[u8],text:&str,cancel:&CancelToken)->Result<Vec<u8>,String>{let result=crate::engine_host::call_with_progress("pdf.subsetFont",&json!({"fontBase64":STANDARD.encode(font),"text":text}),cancel,&|_,_|{}).map_err(|e|e.message)?;let encoded=result.get("fontBase64").and_then(|v|v.as_str()).ok_or("invalid font subset response")?;STANDARD.decode(encoded).map_err(|e|e.to_string())}
fn unique(text:&str)->String {let mut seen=HashSet::new();text.chars().filter(|c|seen.insert(*c)).collect()}

pub(super) fn apply(job:&V,source:&Source,tree:&V,user_data:&Path,progress:&(dyn Fn(f64,f64)+Sync),cancel:&CancelToken)->Result<(Vec<u8>,V),JobFailure>{
 active(cancel)?;let payload=model::field(job,"payload");let lang=match model::field(payload,"lang"){V::String(s) if ["zh","en","ja","ko","fr","de","es","th","id","ru","ar","pt","it","pl","cs","nl","ms","he","hi","zh-TW","vi"].contains(&s.as_str())=>s.as_str(),_=>return Err(JobFailure::new("failed",Some("translated PDF: the payload must name a target language")))};
 let bilingual=matches!(model::field(payload,"bilingual"),V::Bool(true));let options=obj(vec![("lang",string(lang))]);let (translated,translation_report)=translation::apply(tree.clone(),&source.doc_hash,Some(&options),user_data)?;active(cancel)?;
 let mut session=PdfiumSession::open_with_cancel(&source.bytes,None,cancel.clone()).map_err(|e|if cancel.is_cancelled(){JobFailure::new("cancelled",None)}else if e.code==Some(4){JobFailure::new("unsupported",Some("the PDF is encrypted; a translated PDF cannot be written into it"))}else{JobFailure::new("failed",Some(&e.message))})?;
 if pdfium::encrypted(&mut session).map_err(|e|failure(e,cancel))?{return Err(JobFailure::new("unsupported",Some("the PDF is encrypted; a translated PDF cannot be written into it")))}let count=pdfium::page_count(&mut session).map_err(|e|failure(e,cancel))?;if count==0{return Err(JobFailure::new("corrupt",None))}
 let model=model::build(tree,&translated,lang).map_err(|e|failure(e,cancel))?;let mut pages=model.pages.iter().map(|p|p.page).chain(model.units.iter().filter(|r|r.translation.is_some()).flat_map(read::unit_pages)).filter(|p|*p>=1&&*p<=count).collect::<Vec<_>>();pages.sort_unstable();pages.dedup();let readings=read::pages(&mut session,&model,&pages,cancel).map_err(|e|failure(e,cancel))?;
 let mut untranslated:Vec<(usize,V)>=Vec::new();let mut original_pages=Vec::new();let mut kept=HashSet::new();for page in &pages {if let Some((reason,detail))=readings.get(page).and_then(|r|r.kept.as_ref()){kept.insert(*page);let mut fields=vec![("page",V::Number(*page as f64)),("reason",string(*reason))];if let Some(detail)=detail{fields.push(("detail",string(detail)))}original_pages.push(obj(fields));}}
 let mut writable=Vec::new();for rec in &model.units {let Some(text)=rec.translation.as_ref() else{untranslated.push((rec.page,info(rec,Some("not-translated"))));continue};let unit_pages=read::unit_pages(rec);if unit_pages.iter().any(|p|kept.contains(p)){untranslated.push((rec.page,info(rec,Some("page-kept"))));continue}if complex(text){untranslated.push((rec.page,info(rec,Some("script-unsupported"))));continue}let results=unit_pages.iter().flat_map(|p|readings.get(p).and_then(|r|r.parts.get(&rec.key)).into_iter().flatten()).collect::<Vec<_>>();if let Some(reason)=results.iter().find_map(|r|if let read::PartResult::Skip(reason)=r{Some(*reason)}else{None}){untranslated.push((rec.page,info(rec,Some(reason))));continue}let parts=results.into_iter().filter_map(|r|if let read::PartResult::Part(p)=r{Some(p.clone())}else{None}).collect::<Vec<_>>();if parts.is_empty(){untranslated.push((rec.page,info(rec,Some("text-not-found"))));continue}let source_text=parts.iter().map(|p|p.source.as_str()).collect::<Vec<_>>().join(" ");if !parts.iter().all(|p|p.scan)&&!rec.is_cell&&analyze::similarity(&analyze::norm_text(&source_text),&analyze::norm_text(&rec.original))<0.5{untranslated.push((rec.page,info(rec,Some("text-not-found"))));continue}let marker=rec.marker.as_deref().filter(|m|analyze::norm_text(&source_text).starts_with(&analyze::norm_text(m)));writable.push(Writable{rec,parts,text:match marker{Some(m)=>format!("{m} {text}"),None=>text.clone()},face:None});}
 let all=unique(&writable.iter().map(|w|w.text.replace('\n',"")).collect::<String>());let shared=if all.is_empty(){None}else{native_callbacks::writeback_font(&all,lang).filter(|f|font::usable(f,&all))};let mut faces:Vec<Vec<u8>>=Vec::new();let mut texts:Vec<String>=Vec::new();for w in &mut writable {let text=w.text.replace('\n',"");let bytes=shared.clone().or_else(||native_callbacks::writeback_font(&text,lang).filter(|f|font::usable(f,&text)));if let Some(bytes)=bytes{let face=match faces.iter().position(|f|*f==bytes){Some(i)=>i,None=>{faces.push(bytes);texts.push(String::new());faces.len()-1}};texts[face].push_str(&text);w.face=Some(face)}}
 let mut embedded=Vec::new();for (face,text) in faces.iter().zip(&texts){active(cancel)?;let result=subset(face,&unique(&format!("{text} ")),cancel);active(cancel)?;embedded.push(result.ok());}let metrics=faces.iter().map(|face|font::FaceMetrics::new(face)).collect::<Result<Vec<_>,_>>().map_err(|e|failure(e,cancel))?;
 let mut plans=Vec::new();let mut plan_meta=Vec::new();for (index,w) in writable.iter().enumerate(){let Some(face)=w.face.filter(|i|embedded[*i].is_some())else{untranslated.push((w.rec.page,info(w.rec,Some("font-missing"))));continue};let mut size=analyze::median(&w.parts.iter().flat_map(|p|p.sizes.iter().copied()).collect::<Vec<_>>());if size==0.0{size=10.0}let figure=w.rec.figure;let group=if w.rec.is_cell{format!("cell:{}",w.rec.owner_id)}else if let Some(figure)=figure{format!("label:{}:{},{}",figure.page,figure.bounds.x0,figure.bounds.y1)}else{format!("{}:{}",w.rec.role,(size*2.0).round()/2.0)};plans.push(place::PlanBlock{key:w.rec.key.clone(),text:w.text.clone(),boxes:w.parts.iter().map(|p|p.bounds).collect(),pages:w.parts.iter().map(|p|p.page).collect(),size,line_height:line_height(&w.parts,size),align:align(w.rec,&w.parts,&model),font:&metrics[face],group,expandable:w.parts.len()==1&&!w.parts[0].scan&&!w.rec.is_cell,container:figure.filter(|f|f.page==w.parts[0].page).map(|f|f.bounds),alternatives:w.parts.first().and_then(|p|(w.parts.len()==1).then_some(p.area).flatten()).map(|r|vec![vec![r]]).unwrap_or_default()});plan_meta.push((index,face));}
 let spaces=readings.into_iter().filter_map(|(p,r)|r.space.map(|s|(p,s))).collect::<HashMap<_,_>>();let placed=place::place_blocks(&plans,&spaces,0.6).map_err(|e|failure(e,cancel))?;let mut ops:HashMap<usize,pdfium::PageOps>=HashMap::new();let mut overflowed=Vec::new();let mut written=0;for (plan,(index,face)) in plans.iter().zip(plan_meta){let w=&writable[index];let Some(Some(placement))=placed.get(&plan.key) else{overflowed.push(info(w.rec,None));continue};for p in &w.parts{let op=ops.entry(p.page).or_default();op.remove.extend(p.remove.iter().copied());op.paints.extend(p.paints.iter().copied());}for line in &placement.layout.lines{if line.text.is_empty(){continue}let part=&w.parts[line.box_index];ops.entry(part.page).or_default().lines.push(pdfium::DrawLine{text:line.text.clone(),x:line.x,y:line.y,size:placement.layout.size,color:part.color,bold:w.rec.bold,face});}written+=1;}
 let mut fonts=vec![0;embedded.len()];for op in ops.values(){for line in &op.lines{if fonts[line.face]==0{fonts[line.face]=pdfium::load_font(&mut session,embedded[line.face].as_deref().ok_or_else(||failure("missing subset font",cancel))?).map_err(|e|failure(e,cancel))?}}}for (i,page) in pages.iter().enumerate(){active(cancel)?;if let Some(op)=ops.get(page){pdfium::write_page(&mut session,*page,op,&fonts).map_err(|e|failure(e,cancel))?}progress((i+1) as f64,pages.len() as f64);}active(cancel)?;let bytes=if bilingual{pdfium::bilingual(&mut session,&source.bytes)}else{pdfium::save(&mut session)}.map_err(|e|failure(e,cancel))?;active(cancel)?;
 untranslated.sort_by_key(|(page,_)|*page);let writer=obj(vec![("lang",string(lang)),("total",V::Number(model.units.len() as f64)),("translated",V::Number(written as f64)),("overflowed",V::Array(overflowed)),("originalPages",V::Array(original_pages)),("untranslated",V::Array(untranslated.into_iter().map(|(_,v)|v).collect()))]);Ok((bytes,obj(vec![("writer",writer),("translation",translation_report.unwrap_or(V::Null))])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_character_set_preserves_first_occurrence() {
        assert_eq!(unique("가a가 b"), "가a b");
    }

    #[test]
    fn shaping_scripts_are_not_sent_to_unshaped_pdf_text() {
        assert!(complex("مرحبا"));
        assert!(complex("हिन्दी"));
        assert!(!complex("한글 and English"));
    }

    #[test]
    fn line_height_uses_source_baseline_gaps() {
        let part=read::Part {page:1,bounds:layout::Rect{x0:0.0,y0:0.0,x1:30.0,y1:40.0},remove:vec![],source:String::new(),paints:vec![],sizes:vec![10.0],lines:vec![30.0,15.0],area:None,color:[0,0,0],scan:false};
        assert_eq!(line_height(&[part],10.0),1.5);
    }
}
