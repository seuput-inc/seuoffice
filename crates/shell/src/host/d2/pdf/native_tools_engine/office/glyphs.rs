//! Character-level PDFium extraction. Pointers never leave the vendor session.
use std::collections::HashMap;
use serde_json::{Value, json};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use super::fontname::{self, Weight};
use super::geometry::Rect;
use super::ir::PdfChar;
use super::script::script_of;

fn reference(id: u64, offset: usize) -> Value { json!({"ref":id,"offset":offset}) }
fn number(value: &Value) -> Result<f64, PdfiumError> { crate::host::d2::pdf::pdfium_session::read_number(value).ok_or_else(|| PdfiumError::from("PDFium returned a non-numeric scalar".to_owned())) }
fn pointer(value: &Value) -> Result<Option<u64>, PdfiumError> { if value.as_u64()==Some(0) { Ok(None) } else { value.get("ref").and_then(Value::as_u64).filter(|id| *id>0).map(Some).ok_or_else(|| PdfiumError::from("PDFium returned an invalid object reference".to_owned())) } }
fn scalar(s: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<f64, PdfiumError> { number(&s.call(name,args)?) }
fn deref(s: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<Option<u64>, PdfiumError> { pointer(&s.call(name,args)?) }
fn f32_at(bytes: &[u8], pos: usize) -> f64 { f32::from_le_bytes(bytes[pos..pos+4].try_into().expect("fixed PDFium float buffer")) as f64 }
fn f64_at(bytes: &[u8], pos: usize) -> f64 { f64::from_le_bytes(bytes[pos..pos+8].try_into().expect("fixed PDFium double buffer")) }
fn u32_at(bytes: &[u8], pos: usize) -> u32 { u32::from_le_bytes(bytes[pos..pos+4].try_into().expect("fixed PDFium integer buffer")) }
#[derive(Clone, Default)]
struct FontInfo { family: String, italic: bool, weight: Option<Weight> }
fn text_name(s: &mut PdfiumSession, method: &str, handle: u64) -> Result<String, PdfiumError> {
    let size=scalar(s,method,&[reference(handle,0),json!(0),json!(0)])? as usize;
    if size<=1 { return Ok(String::new()); }
    let buf=s.alloc(size)?;
    let result=(|| { scalar(s,method,&[reference(handle,0),reference(buf,0),json!(size)])?; let data=s.read(buf,0,size-1)?; Ok(String::from_utf8_lossy(&data).into_owned()) })();
    let release=s.free(buf); match (result,release) { (Err(e),_)=>Err(e), (Ok(_),Err(e))=>Err(e), (Ok(v),Ok(()))=>Ok(v) }
}
fn font_info(s: &mut PdfiumSession, text_page: u64, index: usize, object: Option<u64>) -> Result<Option<FontInfo>, PdfiumError> {
    let flags=s.alloc(4)?;
    let result=(|| {
        let size=scalar(s,"FPDFText_GetFontInfo",&[reference(text_page,0),json!(index),json!(0),json!(0),reference(flags,0)])? as usize;
        if size==0 { return Ok(None); }
        let buf=s.alloc(size)?;
        let name_result=(|| { scalar(s,"FPDFText_GetFontInfo",&[reference(text_page,0),json!(index),reference(buf,0),json!(size),reference(flags,0)])?; let bytes=s.read(buf,0,size.saturating_sub(1))?; Ok(String::from_utf8_lossy(&bytes).into_owned()) })();
        let release=s.free(buf); let ps=match (name_result,release) { (Err(e),_)=>return Err(e), (Ok(_),Err(e))=>return Err(e), (Ok(v),Ok(()))=>v };
        let flag=u32_at(&s.read(flags,0,4)?,0); let mut family=String::new();
        if let Some(obj)=object { if let Some(font)=deref(s,"FPDFTextObj_GetFont",&[reference(obj,0)])? { family=text_name(s,"FPDFFont_GetFamilyName",font)?; } }
        if family.is_empty() { family=ps.clone(); }
        Ok(Some(FontInfo { family: fontname::family(&family), italic: flag&64!=0 || jscompat::js_regex!(r"italic|oblique","i").test(&ps) || fontname::is_italic(&ps), weight: fontname::weight(&ps) }))
    })();
    let release=s.free(flags); match (result,release) { (Err(e),_)=>Err(e), (Ok(_),Err(e))=>Err(e), (Ok(v),Ok(()))=>Ok(v) }
}

pub(super) fn read_chars(s: &mut PdfiumSession, text_page: u64) -> Result<Vec<PdfChar>, PdfiumError> {
    let count=scalar(s,"FPDFText_CountChars",&[reference(text_page,0)])?;
    if count<=0.0 { return Ok(Vec::new()); }
    let mut allocated=Vec::new();
    let result=(|| {
        for size in [32,16,16,16,24] { allocated.push(s.alloc(size)?); }
        let (d4,d2,u4,f4,f6)=(allocated[0],allocated[1],allocated[2],allocated[3],allocated[4]);
        let mut fonts:HashMap<u64,Option<FontInfo>>=HashMap::new(); let mut modes:HashMap<u64,f64>=HashMap::new(); let mut serials:HashMap<u64,usize>=HashMap::new(); let mut last_font=FontInfo::default(); let mut chars=Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let p=reference(text_page,0); let idx=json!(i);
            let requests = [
                ("FPDFText_GetUnicode",vec![p.clone(),idx.clone()]),
                ("FPDFText_GetCharBox",vec![p.clone(),idx.clone(),reference(d4,0),reference(d4,8),reference(d4,16),reference(d4,24)]),
                ("FPDFText_GetLooseCharBox",vec![p.clone(),idx.clone(),reference(f4,0)]),
                ("FPDFText_GetCharOrigin",vec![p.clone(),idx.clone(),reference(d2,0),reference(d2,8)]),
                ("FPDFText_GetTextObject",vec![p.clone(),idx.clone()]),
                ("FPDFText_GetFillColor",vec![p.clone(),idx.clone(),reference(u4,0),reference(u4,4),reference(u4,8),reference(u4,12)]),
                ("FPDFText_GetMatrix",vec![p.clone(),idx.clone(),reference(f6,0)]),
                ("FPDFText_GetFontWeight",vec![p.clone(),idx.clone()]),
                ("FPDFText_GetCharAngle",vec![p.clone(),idx.clone()]),
                ("FPDFText_GetFontSize",vec![p.clone(),idx.clone()]),
                ("FPDFText_IsGenerated",vec![p.clone(),idx.clone()]),
                ("FPDFText_IsHyphen",vec![p,idx]),
            ];
            let values=s.batch(&requests)?; if values.len()!=requests.len() { return Err(PdfiumError::from("PDFium glyph result count mismatch".into())); }
            let code=number(&values[0])? as u32; if number(&values[1])?==0.0 { continue; }
            let b=s.read(d4,0,32)?; let bounds=Rect { x0:f64_at(&b,0),x1:f64_at(&b,8),y0:f64_at(&b,16),y1:f64_at(&b,24) };
            let loose=if number(&values[2])?!=0.0 { let b=s.read(f4,0,16)?; Rect { x0:f32_at(&b,0),y1:f32_at(&b,4),x1:f32_at(&b,8),y0:f32_at(&b,12) } } else { bounds };
            let (x,y)=if number(&values[3])?!=0.0 { let b=s.read(d2,0,16)?; (f64_at(&b,0),f64_at(&b,8)) } else { (bounds.x0,bounds.y0) };
            let object=pointer(&values[4])?; let key=object.unwrap_or(0); let next=serials.len(); let serial=*serials.entry(key).or_insert(next);
            if !fonts.contains_key(&key) { let font=font_info(s,text_page,i,object)?; fonts.insert(key,font); }
            if let Some(Some(info))=fonts.get(&key) { last_font=info.clone(); }
            let info=fonts.get(&key).and_then(|v| v.as_ref()).unwrap_or(&last_font);
            let mode=if let Some(mode)=modes.get(&key) { *mode } else { let value=if let Some(obj)=object { scalar(s,"FPDFTextObj_GetTextRenderMode",&[reference(obj,0)])? } else { -1.0 }; modes.insert(key,value); value };
            let color=if number(&values[5])?!=0.0 { let b=s.read(u4,0,16)?; format!("{:02X}{:02X}{:02X}",u32_at(&b,0),u32_at(&b,4),u32_at(&b,8)) } else { "000000".into() };
            let (mut hs,mut vs)=(1.0,1.0); if number(&values[6])?!=0.0 { let b=s.read(f6,0,24)?; let sx=f32_at(&b,0).hypot(f32_at(&b,4)); let sy=f32_at(&b,8).hypot(f32_at(&b,12)); if sx>0.0 && sy>0.0 { hs=sx/sy; vs=sy; } }
            let weight=number(&values[7])?; let weight=match info.weight { Some(Weight::Bold)=>weight.max(700.0), Some(Weight::Regular)=>400.0, None if weight>0.0=>weight, None=>400.0 };
            let angle=number(&values[8])?; let angle=angle-2.0*std::f64::consts::PI*jscompat::math_round(angle/(2.0*std::f64::consts::PI));
            let text=if code==0 { String::new() } else { char::from_u32(code).ok_or_else(|| PdfiumError::from(format!("invalid PDFium Unicode code point {code}")))?.to_string() };
            chars.push(PdfChar { code,text,bounds,loose_box:loose,origin_x:x,origin_y:y,angle,font_size:number(&values[9])?*vs,font_weight:weight,font_family:info.family.clone(),italic:info.italic,color,hscale:Some(hs),invisible:mode==3.0 || mode==7.0,is_generated:number(&values[10])?==1.0,pdf_char_index:Some(i),text_object_id:Some(serial),is_hyphen:number(&values[11])?==1.0,script:script_of(code),highlight:None,underline:false,strike:false,note_ref:None,href:None });
        }
        Ok(chars)
    })();
    let mut cleanup_error=None; for id in allocated { if let Err(e)=s.free(id) { if cleanup_error.is_none() { cleanup_error=Some(e); } } }
    match (result,cleanup_error) { (Err(e),_)=>Err(e), (Ok(_),Some(e))=>Err(e), (Ok(v),None)=>Ok(v) }
}
