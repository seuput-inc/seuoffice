//! Permanent PDFium redaction with preflight against recoverable image and form data.

use std::collections::{HashMap, HashSet};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shell_contract::{IpcValue, runtime::CancelToken};
use super::super::pdfium_session::PdfiumSession;

fn field<'a>(v: &'a IpcValue, key: &str) -> Option<&'a IpcValue> { match v { IpcValue::Object(fields) => fields.iter().find(|(name, _)| name == key).map(|(_, value)| value), _ => None } }
fn num(v: Option<&IpcValue>) -> Option<f64> { match v { Some(IpcValue::Number(value)) => Some(*value), _ => None } }
fn r(id: u64) -> Value { json!({"ref":id}) }
fn offset(id: u64, n: usize) -> Value { json!({"ref":id,"offset":n}) }
fn call(s: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<Value, String> { s.call(name, args).map_err(|e| e.to_string()) }
fn integer(v: Value) -> Result<i64, String> { v.as_i64().ok_or_else(|| "invalid PDFium response".into()) }
fn handle(v: Value) -> Option<u64> { v.get("ref").and_then(Value::as_u64).filter(|id| *id != 0) }
fn success(v: Value) -> bool { v.as_i64().is_some_and(|n| n != 0) }
fn read_floats(s: &mut PdfiumSession, id: u64) -> Result<[f64; 4], String> {
    let b = s.read(id, 0, 16).map_err(|e| e.to_string())?;
    let mut out = [0.0; 4];
    for (index, value) in out.iter_mut().enumerate() { *value = f32::from_le_bytes(b[index*4..index*4+4].try_into().map_err(|_| "invalid PDFium rectangle")?) as f64; }
    Ok(out)
}
fn intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0].min(a[2]) < b[0].max(b[2]) && a[0].max(a[2]) > b[0].min(b[2]) &&
        a[1].min(a[3]) < b[1].max(b[3]) && a[1].max(a[3]) > b[1].min(b[3])
}
fn page(s: &mut PdfiumSession, doc: u64, index: usize) -> Result<u64, String> {
    handle(call(s, "FPDF_LoadPage", &[r(doc), json!(index)])?).ok_or_else(|| format!("PDFium could not load redaction page {}", index+1))
}
fn annotation_rect(s: &mut PdfiumSession, annot: u64, index: usize) -> Result<[f64;4], String> {
    let buffer = s.alloc(16).map_err(|e| e.to_string())?;
    let ok = success(call(s, "FPDFAnnot_GetRect", &[r(annot), r(buffer)])?);
    let bounds = if ok { Some(read_floats(s, buffer)?) } else { None };
    s.free(buffer).map_err(|e| e.to_string())?;
    bounds.ok_or_else(|| format!("PDFium could not inspect annotation on page {}", index+1))
}
fn object_bounds(s: &mut PdfiumSession, object: u64) -> Result<Option<[f64;4]>, String> {
    let buffer = s.alloc(16).map_err(|e| e.to_string())?;
    let ok = success(call(s, "FPDFPageObj_GetBounds", &[r(object), offset(buffer,0), offset(buffer,4), offset(buffer,8), offset(buffer,12)])?);
    let bounds = if ok { Some(read_floats(s, buffer)?) } else { None };
    s.free(buffer).map_err(|e| e.to_string())?;
    Ok(bounds)
}
fn image_hash(s: &mut PdfiumSession, object: u64) -> Result<[u8;32], String> {
    let len = integer(call(s, "FPDFImageObj_GetImageDataRaw", &[r(object), json!(0), json!(0)])?)?;
    let len = usize::try_from(len).ok().filter(|value| *value > 0).ok_or("PDFium could not inspect image data safely")?;
    let buffer = s.alloc(len).map_err(|e| e.to_string())?;
    let written = integer(call(s, "FPDFImageObj_GetImageDataRaw", &[r(object), r(buffer), json!(len)])?)?;
    if written != len as i64 { return Err("PDFium could not inspect image data safely".into()); }
    let bytes = s.read(buffer,0,len).map_err(|e| e.to_string())?;
    s.free(buffer).map_err(|e| e.to_string())?;
    Ok(Sha256::digest(bytes).into())
}
fn image_census(s: &mut PdfiumSession, object: u64, counts: &mut HashMap<[u8;32], usize>, seen: &mut HashSet<u64>) -> Result<bool, String> {
    if !seen.insert(object) { return Ok(false); }
    match integer(call(s, "FPDFPageObj_GetType", &[r(object)])?)? {
        3 => { *counts.entry(image_hash(s,object)?).or_insert(0) += 1; Ok(true) }
        5 => {
            let count = integer(call(s, "FPDFFormObj_CountObjects", &[r(object)])?)?;
            let mut any = false;
            for i in 0..count {
                if let Some(child) = handle(call(s, "FPDFFormObj_GetObject", &[r(object), json!(i)])?) {
                    any |= image_census(s, child, counts, seen)?;
                }
            }
            Ok(any)
        }
        _ => Ok(false),
    }
}
fn check_page(s: &mut PdfiumSession, page: u64, index: usize, region: [f64;4], counts: &HashMap<[u8;32],usize>) -> Result<(), String> {
    let count = integer(call(s, "FPDFPage_CountObjects", &[r(page)])?)?;
    for i in 0..count {
        let Some(object) = handle(call(s, "FPDFPage_GetObject", &[r(page), json!(i)])?) else { continue };
        let Some(bounds) = object_bounds(s,object)? else { continue };
        if !intersects(region,bounds) { continue; }
        match integer(call(s, "FPDFPageObj_GetType", &[r(object)])?)? {
            3 if counts.get(&image_hash(s,object)?).copied().unwrap_or(0) > 1 => return Err(format!("redaction intersects a shared image on page {}; refusing unsafe image mutation",index+1)),
            5 => { let mut ignored=HashMap::new(); if image_census(s,object,&mut ignored,&mut HashSet::new())? { return Err(format!("redaction intersects an image Form XObject on page {}; refusing unsafe image mutation",index+1)); } }
            _ => {}
        }
    }
    let count = integer(call(s, "FPDFPage_GetAnnotCount", &[r(page)])?)?;
    for i in 0..count {
        let annot = handle(call(s,"FPDFPage_GetAnnot",&[r(page),json!(i)])?).ok_or_else(|| format!("PDFium could not inspect annotation on page {}",index+1))?;
        let subtype = integer(call(s,"FPDFAnnot_GetSubtype",&[r(annot)])?)?;
        let hit = if subtype == 20 { intersects(region,annotation_rect(s,annot,index)?) } else { false };
        call(s,"FPDFPage_CloseAnnot",&[r(annot)])?;
        if hit { return Err(format!("redaction intersects a form field on page {}; refusing recoverable field data",index+1)); }
    }
    Ok(())
}
fn remove_annotations(s: &mut PdfiumSession, page: u64, index: usize, region: [f64;4]) -> Result<(), String> {
    let count = integer(call(s,"FPDFPage_GetAnnotCount",&[r(page)])?)?;
    for i in (0..count).rev() {
        let annot = handle(call(s,"FPDFPage_GetAnnot",&[r(page),json!(i)])?).ok_or_else(||format!("PDFium could not inspect annotation on page {}",index+1))?;
        let hit = intersects(region,annotation_rect(s,annot,index)?);
        call(s,"FPDFPage_CloseAnnot",&[r(annot)])?;
        if hit && !success(call(s,"FPDFPage_RemoveAnnot",&[r(page),json!(i)])?) { return Err(format!("PDFium could not remove annotation on page {}",index+1)); }
    }
    Ok(())
}

pub(super) fn apply(bytes: &[u8], regions: &[IpcValue], cancel: &CancelToken) -> Result<Vec<u8>, String> {
    if regions.is_empty() || regions.len() > 500 { return Err("invalid redaction regions".into()); }
    let mut parsed = Vec::with_capacity(regions.len());
    for region in regions {
        let index = num(field(region,"pageIndex")).filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0).ok_or("invalid redaction page")?;
        let Some(IpcValue::Array(rect)) = field(region,"rect") else { return Err("invalid redaction rectangle".into()) };
        if rect.len() != 4 { return Err("invalid redaction rectangle".into()); }
        let mut coords = [0.0;4];
        for (i, value) in rect.iter().enumerate() { coords[i] = num(Some(value)).filter(|n| n.is_finite() && n.abs() <= 3.402823466e38).ok_or("invalid redaction rectangle")?; }
        if coords[0] == coords[2] || coords[1] == coords[3] { return Err("invalid redaction rectangle".into()); }
        parsed.push((index as usize,[coords[0].min(coords[2]),coords[1].min(coords[3]),coords[0].max(coords[2]),coords[1].max(coords[3])]));
    }
    let mut session = PdfiumSession::open_with_cancel(bytes,None,cancel.clone()).map_err(|e|e.to_string())?;
    let doc = session.document();
    let count = usize::try_from(integer(call(&mut session,"FPDF_GetPageCount",&[r(doc)])?)?).map_err(|_|"invalid PDF page count")?;
    if parsed.iter().any(|(index,_)| *index >= count) { return Err("redaction page index out of range".into()); }
    let mut counts = HashMap::new();
    for index in 0..count {
        let page = page(&mut session,doc,index)?;
        let total = integer(call(&mut session,"FPDFPage_CountObjects",&[r(page)])?)?;
        for i in 0..total { if let Some(object) = handle(call(&mut session,"FPDFPage_GetObject",&[r(page),json!(i)])?) { image_census(&mut session,object,&mut counts,&mut HashSet::new())?; } }
        call(&mut session,"FPDF_ClosePage",&[r(page)])?;
    }
    for (index, rect) in &parsed {
        let page = page(&mut session,doc,*index)?;
        check_page(&mut session,page,*index,*rect,&counts)?;
        call(&mut session,"FPDF_ClosePage",&[r(page)])?;
    }
    for (index, rect) in parsed {
        let page = page(&mut session,doc,index)?;
        remove_annotations(&mut session,page,index,rect)?;
        let annot = handle(call(&mut session,"FPDFPage_CreateAnnot",&[r(page),json!(28)])?).ok_or_else(||format!("PDFium could not create redaction on page {}",index+1))?;
        let buffer = session.alloc(16).map_err(|e|e.to_string())?;
        let mut raw = [0u8;16];
        for (i,value) in rect.iter().enumerate() { raw[i*4..i*4+4].copy_from_slice(&(*value as f32).to_le_bytes()); }
        session.write(buffer,0,&raw).map_err(|e|e.to_string())?;
        if !success(call(&mut session,"FPDFAnnot_SetRect",&[r(annot),r(buffer)])?) { return Err(format!("PDFium rejected redaction rectangle on page {}",index+1)); }
        s_free(&mut session,buffer)?;
        if !success(call(&mut session,"EPDFAnnot_SetColor",&[r(annot),json!(2),json!(0),json!(0),json!(0)])?) { return Err(format!("PDFium could not style redaction on page {}",index+1)); }
        if !success(call(&mut session,"EPDFAnnot_ApplyRedaction",&[r(page),r(annot)])?) { return Err(format!("PDFium could not apply redaction on page {}",index+1)); }
        let blackout = handle(call(&mut session,"FPDFPageObj_CreateNewRect",&[json!(rect[0]),json!(rect[3]),json!(rect[2]-rect[0]),json!(rect[1]-rect[3])])?).ok_or("PDFium could not create blackout")?;
        if !success(call(&mut session,"FPDFPageObj_SetFillColor",&[r(blackout),json!(0),json!(0),json!(0),json!(255)])?) || !success(call(&mut session,"FPDFPath_SetDrawMode",&[r(blackout),json!(1),json!(0)])?) { return Err("PDFium could not create blackout".into()); }
        call(&mut session,"FPDFPage_InsertObject",&[r(page),r(blackout)])?;
        call(&mut session,"FPDFPage_CloseAnnot",&[r(annot)])?;
        if !success(call(&mut session,"FPDFPage_GenerateContent",&[r(page)])?) { return Err(format!("PDFium could not serialize redaction on page {}",index+1)); }
        call(&mut session,"FPDF_ClosePage",&[r(page)])?;
    }
    let writer = handle(call(&mut session,"PDFiumExt_OpenFileWriter",&[])?).ok_or("PDFium could not open writer")?;
    if !success(call(&mut session,"PDFiumExt_SaveAsCopy",&[r(doc),r(writer)])?) { return Err("PDFium SaveAsCopy failed".into()); }
    let size = usize::try_from(integer(call(&mut session,"PDFiumExt_GetFileWriterSize",&[r(writer)])?)?).map_err(|_|"invalid PDFium writer size")?;
    let buffer = session.alloc(size).map_err(|e|e.to_string())?;
    call(&mut session,"PDFiumExt_GetFileWriterData",&[r(writer),r(buffer),json!(size)])?;
    let output = session.read(buffer,0,size).map_err(|e|e.to_string())?;
    s_free(&mut session,buffer)?;
    call(&mut session,"PDFiumExt_CloseFileWriter",&[r(writer)])?;
    Ok(output)
}
fn s_free(s: &mut PdfiumSession, id: u64) -> Result<(), String> { s.free(id).map_err(|e| e.to_string()) }
