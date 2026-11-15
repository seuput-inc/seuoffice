//! Map source page objects and recognized scan lines back to translation units.
use std::collections::HashMap;
use shell_contract::runtime::CancelToken;
use image::{ImageEncoder, ExtendedColorType, codecs::png::PngEncoder};
use super::{analyze::{self, CellResult, FormChar, OcrLine, TextObject}, bitmap::{self, PageBitmap}, frame::PageFrame, layout::Rect, model::{Model, UnitRec}, pdfium, place::PageSpace};
use super::super::super::super::pdfium_session::PdfiumSession;
use crate::host::d2::pdf::native_callbacks;

#[derive(Clone)]
pub(super) struct Part { pub page:usize, pub bounds:Rect, pub remove:Vec<usize>, pub source:String, pub paints:Vec<(Rect,[u8;3])>, pub sizes:Vec<f64>, pub lines:Vec<f64>, pub area:Option<Rect>, pub color:[u8;3], pub scan:bool }
#[derive(Clone)]
pub(super) enum PartResult { Part(Part), Skip(&'static str) }
#[derive(Default)]
pub(super) struct Reading { pub kept:Option<(&'static str,Option<String>)>, pub space:Option<PageSpace>, pub parts:HashMap<String,Vec<PartResult>> }
pub(super) fn unit_pages(rec:&UnitRec)->Vec<usize> { let mut pages=Vec::new(); for region in &rec.regions {if !pages.contains(&region.page){pages.push(region.page)}} if pages.is_empty(){pages.push(rec.page)} pages }
fn lines(mut ys:Vec<f64>,size:f64)->Vec<f64>{ys.sort_by(|a,b|b.total_cmp(a));let mut out=Vec::new();for y in ys{if out.last().is_none_or(|p:&f64|*p-y>size*0.3){out.push(y)}}out}
fn clip(r:Rect,to:Rect)->Rect {Rect{x0:r.x0.max(to.x0),y0:r.y0.max(to.y0),x1:r.x1.min(to.x1),y1:r.y1.min(to.y1)}}
fn text_part(page:usize,bounds:Rect,objects:&[TextObject],indices:&[usize],area:Option<Rect>,bitmap:Option<&PageBitmap>)->Part {
 let ordered=analyze::reading_order(objects,indices);let sizes=indices.iter().map(|&i|objects[i].size).collect::<Vec<_>>();let size=analyze::median(&sizes);
 let mut part=Part{page,bounds,remove:indices.iter().map(|&i|objects[i].index).collect(),source:ordered.iter().map(|&i|objects[i].text.as_str()).collect::<Vec<_>>().join(" "),paints:Vec::new(),sizes,lines:lines(indices.iter().flat_map(|&i|objects[i].baselines.iter().copied()).collect(),size),area,color:indices.iter().find_map(|&i|(!objects[i].invisible).then_some(objects[i].color)).unwrap_or([0,0,0]),scan:false};
 if indices.iter().all(|&i|objects[i].invisible){if let Some(bitmap)=bitmap {part.scan=true;for &i in indices {let r=objects[i].bounds;let paint=analyze::expand(r,1.0_f64.max((r.y1-r.y0)*0.15));part.paints.push((paint,bitmap::sample_background(bitmap,paint)));}if let Some(bounds)=analyze::union(&indices.iter().map(|&i|objects[i].bounds).collect::<Vec<_>>()){part.color=bitmap::readable_ink(bitmap::sample_ink(bitmap,bounds),part.paints[0].1);}}} part
}
fn cover_part(page:usize,bounds:Rect,chars:&[FormChar],bitmap:&PageBitmap)->Part {let sizes=chars.iter().map(|c|c.size).collect::<Vec<_>>();let size=analyze::median(&sizes);let rect=analyze::expand(bounds,0.5_f64.max((bounds.y1-bounds.y0)*0.1));let bg=bitmap::sample_background(bitmap,rect);Part{page,bounds,remove:vec![],source:String::new(),paints:vec![(rect,bg)],sizes,lines:lines(chars.iter().map(|c|c.at.1).collect(),size),area:None,color:bitmap::readable_ink(bitmap::sample_ink(bitmap,bounds),bg),scan:true}}
fn scan_part(page:usize,bounds:Rect,selected:&[OcrLine],original:&str,bitmap:&PageBitmap)->Part {
 let share=if jscompat::js_regex!(r"[\u{3040}-\u{30ff}\u{3400}-\u{9fff}\u{ac00}-\u{d7af}\u{f900}-\u{faff}]","u").test(original){0.9}else{0.72};
 let rects=if selected.is_empty(){vec![bounds]}else{selected.iter().map(|l|l.bounds).collect()};let sizes=rects.iter().map(|r|(r.y1-r.y0)*share).collect();let paints=rects.iter().map(|r|{let r=analyze::expand(*r,1.0_f64.max((r.y1-r.y0)*0.12));(r,bitmap::sample_background(bitmap,r))}).collect::<Vec<_>>();let color=bitmap::readable_ink(bitmap::sample_ink(bitmap,analyze::union(&rects).unwrap_or(bounds)),paints[0].1);Part{page,bounds,remove:vec![],source:String::new(),paints,sizes,lines:lines(selected.iter().map(|l|analyze::center(l.bounds).1).collect(),0.0),area:None,color,scan:true}
}
fn scan(session:&mut PdfiumSession,page:u64,page_no:usize,frame:PageFrame,on:&[&UnitRec],every:&[&UnitRec],cancel:&CancelToken)->Result<Reading,String>{
 let mut out=Reading::default();let Some(bitmap)=pdfium::render_bitmap(session,page,frame,3.0)? else {out.kept=Some(("ocr-unavailable",None));return Ok(out)};
 let mut png=Vec::new();PngEncoder::new(&mut png).write_image(&bitmap.rgba,u32::try_from(bitmap.width).map_err(|e|e.to_string())?,u32::try_from(bitmap.height).map_err(|e|e.to_string())?,ExtendedColorType::Rgba8).map_err(|e|e.to_string())?;
 let recognition=match native_callbacks::recognize_page(&png,frame.width,frame.height,cancel){Ok(v)=>v,Err(e) if cancel.is_cancelled()=>return Err(e),Err(_)=>None};
 let mut found=Vec::new();if let Some(value)=recognition {if let Some(items)=value.get("lines").and_then(|v|v.as_array()){for line in items {let Some(text)=line.get("text").and_then(|v|v.as_str()) else{continue};if text.trim().is_empty()||line.get("confidence").and_then(|v|v.as_f64()).unwrap_or(0.0)<0.3{continue}let Some(b)=line.get("box") else{continue};if let (Some(x0),Some(y0),Some(x1),Some(y1))=(b.get("x0").and_then(|v|v.as_f64()),b.get("y0").and_then(|v|v.as_f64()),b.get("x1").and_then(|v|v.as_f64()),b.get("y1").and_then(|v|v.as_f64())){found.push(OcrLine{text:text.into(),bounds:Rect{x0:x0*frame.width,y0:y0*frame.height,x1:x1*frame.width,y1:y1*frame.height}})}}}}
 if found.is_empty(){out.kept=Some(("ocr-unavailable",None));return Ok(out)}
 let unplaced=every.iter().copied().filter(|r|r.regions.is_empty()).collect::<Vec<_>>();let matched=analyze::match_lines_to_blocks(&found,&unplaced.iter().map(|r|r.original.clone()).collect::<Vec<_>>());
 for rec in on {let parts=if !rec.regions.is_empty(){rec.regions.iter().filter(|r|r.page==page_no).map(|r|{let selected=analyze::lines_in_region(&found,r.bounds).into_iter().map(|i|found[i].clone()).collect::<Vec<_>>();PartResult::Part(scan_part(page_no,r.bounds,&selected,&rec.original,&bitmap))}).collect()}else if let Some(index)=unplaced.iter().position(|u|u.key==rec.key){if let Some(ids)=matched.get(&index){let selected=ids.iter().map(|&i|found[i].clone()).collect::<Vec<_>>();let bounds=analyze::union(&selected.iter().map(|l|l.bounds).collect::<Vec<_>>()).ok_or("invalid OCR placement")?;vec![PartResult::Part(scan_part(page_no,bounds,&selected,&rec.original,&bitmap))]}else{vec![PartResult::Skip("no-position")]}}else{vec![PartResult::Skip("no-position")]};out.parts.insert(rec.key.clone(),parts);}
 out.space=Some(PageSpace{text:vec![],shapes:vec![],content:Rect{x0:0.0,y0:0.0,x1:0.0,y1:0.0}});Ok(out)
}
fn text_page(session:&mut PdfiumSession,page:u64,page_no:usize,frame:PageFrame,model:&Model,on:&[&UnitRec])->Result<Reading,String>{
 let data=pdfium::read_page(session,page,frame)?;let objects=&data.text;let owners=model.owners_on(page_no);let mut owned=analyze::assign_objects(objects,&owners);let adopted=analyze::adopt_hyphens(objects,&owners,&mut owned);let mut bitmap=None;let mut rendered=false;let mut out=Reading::default();let mut tables:HashMap<String,HashMap<String,CellResult>>=HashMap::new();
 for rec in on {let mut result=Vec::new();if rec.regions.is_empty(){result.push(PartResult::Skip("no-position"));}for (i,region) in rec.regions.iter().enumerate(){if region.page!=page_no{continue}let key=format!("{}\u{1}{}",rec.owner_id,i);let owns=owned.get(&key).map(Vec::as_slice).unwrap_or(&[]);let upright=owns.iter().copied().filter(|&i|rec.is_cell||!objects[i].vertical).collect::<Vec<_>>();if !owns.is_empty()&&upright.is_empty(){result.push(PartResult::Skip("vertical-text"));continue}let bounds=analyze::union(&upright.iter().map(|&i|objects[i].bounds).collect::<Vec<_>>()).map(|b|clip(region.bounds,analyze::expand(b,1.0))).unwrap_or(region.bounds);
 if rec.is_cell {if !tables.contains_key(&key){let cells=model.cells.get(&rec.owner_id).map(Vec::as_slice).unwrap_or(&[]);let placed=if cells.iter().all(|c|c.area.is_some()){analyze::place_cells_by_area(objects,&upright,cells)?}else{analyze::place_cells(objects,&upright,cells,region.bounds).into_iter().map(|(k,v)|(k,CellResult::Placed(v))).collect()};tables.insert(key.clone(),placed);}match tables.get(&key).and_then(|t|t.get(&rec.key)){Some(CellResult::Shared)=>result.push(PartResult::Skip("shared-text-object")),None=>result.push(PartResult::Skip("text-not-found")),Some(CellResult::Placed(cell))=>{if cell.objects.iter().any(|&i|objects[i].vertical){result.push(PartResult::Skip("vertical-text"))}else{if cell.objects.iter().all(|&i|objects[i].invisible)&&!rendered{bitmap=pdfium::render_bitmap(session,page,frame,2.0)?;rendered=true}result.push(PartResult::Part(text_part(page_no,cell.bounds,objects,&cell.objects,cell.area,bitmap.as_ref())))}}}continue}
 let form=data.form_chars.iter().copied().filter(|c|analyze::contains(analyze::expand(bounds,1.0),c.at)).collect::<Vec<_>>();if upright.is_empty()&&!form.is_empty()&&rec.role=="label" {if !rendered{bitmap=pdfium::render_bitmap(session,page,frame,2.0)?;rendered=true}result.push(match bitmap.as_ref(){Some(b)=>PartResult::Part(cover_part(page_no,bounds,&form,b)),None=>PartResult::Skip("text-in-form")});}else if upright.is_empty(){result.push(PartResult::Skip("text-not-found"))}else if !form.is_empty(){result.push(PartResult::Skip("text-in-form"))}else if !upright.iter().all(|i|adopted.contains(i)||analyze::mostly_inside(objects[*i].bounds,analyze::expand(bounds,2.0))){result.push(PartResult::Skip("shared-text-object"))}else{if upright.iter().all(|&i|objects[i].invisible)&&!rendered{bitmap=pdfium::render_bitmap(session,page,frame,2.0)?;rendered=true}result.push(PartResult::Part(text_part(page_no,bounds,objects,&upright,None,bitmap.as_ref())))}
 }out.parts.insert(rec.key.clone(),result);}
 let text=objects.iter().map(|o|o.bounds).collect::<Vec<_>>();let content=analyze::union(&text).unwrap_or(Rect{x0:0.0,y0:0.0,x1:frame.width,y1:frame.height});out.space=Some(PageSpace{text,shapes:data.shapes,content});Ok(out)
}
pub(super) fn pages(session:&mut PdfiumSession,model:&Model,pages:&[usize],cancel:&CancelToken)->Result<HashMap<usize,Reading>,String>{let mut out=HashMap::new();for &page_no in pages {if cancel.is_cancelled(){return Err("cancelled".into())}let info=model.pages.iter().find(|p|p.page==page_no);let status=info.map(|p|p.status.as_str()).unwrap_or("text");let every=model.units.iter().filter(|r|unit_pages(r).contains(&page_no)).collect::<Vec<_>>();let on=every.iter().copied().filter(|r|r.translation.is_some()).collect::<Vec<_>>();let kept=match status {"toc"=>Some(("toc-page",None)),"scanned"=>Some(("scanned-no-text",None)),"degraded"=>Some(if info.and_then(|p|p.reason.as_deref())==Some("vertical-text"){("vertical-text",None)}else{("not-editable",info.and_then(|p|p.reason.clone()))}),_=>None};if let Some(kept)=kept{out.insert(page_no,Reading{kept:Some(kept),..Reading::default()});continue}if on.is_empty(){continue}let page=pdfium::load_page(session,page_no)?;let result=(||{let frame=pdfium::frame(session,page)?;if status=="ocr"||status=="ai"{scan(session,page,page_no,frame,&on,&every,cancel)}else{text_page(session,page,page_no,frame,model,&on)}})();let closed=pdfium::close_page(session,page);let reading=result?;closed?;out.insert(page_no,reading);}Ok(out)}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_baselines_are_deduplicated_top_down() {
        assert_eq!(lines(vec![10.0, 30.0, 29.0, 20.0], 10.0), vec![30.0, 20.0, 10.0]);
    }

    #[test]
    fn unmeasured_units_use_their_start_page() {
        let unit=UnitRec {key:"p".into(),block_id:"p".into(),page:3,owner_id:"p".into(),is_cell:false,regions:vec![],title_like:false,role:"text".into(),marker:None,original:"source".into(),translation:Some("translation".into()),bold:false,figure:None};
        assert_eq!(unit_pages(&unit),vec![3]);
    }
}
