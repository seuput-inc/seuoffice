//! Image-object payload selection, mask rendering, and original JPEG preservation.
use serde_json::json;
use crate::host::d2::pdf::pdfium_session::{PdfiumError,PdfiumSession};
use super::bitmap::{self,Rgba};
use super::geometry::Rect;
use super::ir::ImageBlock;
use super::paths::{ptr,number,object};

#[derive(Clone,Copy)]
pub(super) struct Crop { pub left:f64,pub top:f64,pub right:f64,pub bottom:f64 }
pub(super) fn rendered(s:&mut PdfiumSession,doc:u64,page:u64,obj:u64,requested:f64)->Result<Option<Rgba>,PdfiumError> {
    let scratch=s.alloc(24)?;
    let result=(|| {
        let present=number(s,"FPDFPageObj_GetMatrix",&[ptr(obj,0),ptr(scratch,0)])?!=0.0;
        let original=if present { Some(s.read(scratch,0,24)?) } else { None };
        let mut scale=requested;
        if let Some(bytes)=&original {
            let f=|i| f32::from_le_bytes(bytes[i..i+4].try_into().expect("PDFium matrix buffer")) as f64;
            let area=f(0).hypot(f(4))*f(8).hypot(f(12));
            if area*scale*scale>24_000_000.0 { scale=(24_000_000.0/area.max(1.0)).sqrt(); }
        }
        let restore=if scale!=1.0 { if let Some(bytes)=&original {
            let mut changed=bytes.clone();
            for i in (0..16).step_by(4) { let value=f32::from_le_bytes(bytes[i..i+4].try_into().expect("PDFium matrix buffer")); changed[i..i+4].copy_from_slice(&((value as f64*scale) as f32).to_le_bytes()); }
            s.write(scratch,0,&changed)?; number(s,"FPDFPageObj_SetMatrix",&[ptr(obj,0),ptr(scratch,0)])?; true
        } else { false } } else { false };
        let image=(|| {
            let Some(bitmap)=object(s,"FPDFImageObj_GetRenderedBitmap",&[ptr(doc,0),ptr(page,0),ptr(obj,0)])? else { return Ok(None) };
            let decoded=bitmap::bitmap_to_rgba(s,bitmap); let destroyed=s.call("FPDFBitmap_Destroy",&[ptr(bitmap,0)]);
            match (decoded,destroyed) { (Err(e),_) | (Ok(_),Err(e))=>Err(e), (Ok(v),Ok(_))=>Ok(v) }
        })();
        let restored=if restore { s.write(scratch,0,original.as_ref().expect("matrix retained"))?; s.call("FPDFPageObj_SetMatrix",&[ptr(obj,0),ptr(scratch,0)]).map(|_| ()) } else { Ok(()) };
        match (image,restored) { (Err(e),_) | (Ok(_),Err(e))=>Err(e), (Ok(v),Ok(()))=>Ok(v) }
    })();
    let freed=s.free(scratch); match (result,freed) { (Err(e),_) | (Ok(_),Err(e))=>Err(e), (Ok(v),Ok(()))=>Ok(v) }
}
fn filters(s:&mut PdfiumSession,obj:u64)->Result<Vec<String>,PdfiumError> {
    let n=number(s,"FPDFImageObj_GetImageFilterCount",&[ptr(obj,0)])? as i32; let mut result=Vec::new();
    for i in 0..n {
        let len=number(s,"FPDFImageObj_GetImageFilter",&[ptr(obj,0),json!(i),json!(0),json!(0)])? as usize;
        if len==0 { continue; }
        let buffer=s.alloc(len)?;
        let value=(|| { number(s,"FPDFImageObj_GetImageFilter",&[ptr(obj,0),json!(i),ptr(buffer,0),json!(len)])?; Ok(String::from_utf8_lossy(&s.read(buffer,0,len-1)?).into_owned()) })();
        let free=s.free(buffer); match (value,free) { (Err(e),_) | (Ok(_),Err(e))=>return Err(e), (Ok(v),Ok(()))=>result.push(v) }
    }
    Ok(result)
}
fn raw_jpeg(s:&mut PdfiumSession,obj:u64)->Result<Option<Vec<u8>>,PdfiumError> {
    let size=number(s,"FPDFImageObj_GetImageDataRaw",&[ptr(obj,0),json!(0),json!(0)])? as usize;
    if size==0 { return Ok(None); }
    let buffer=s.alloc(size)?;
    let result=(|| { number(s,"FPDFImageObj_GetImageDataRaw",&[ptr(obj,0),ptr(buffer,0),json!(size)])?; let bytes=s.read(buffer,0,size)?; Ok(matches!(bitmap::jpeg_sof_components(&bytes),1|3).then_some(bytes)) })();
    let free=s.free(buffer); match (result,free) { (Err(e),_) | (Ok(_),Err(e))=>Err(e), (Ok(v),Ok(()))=>Ok(v) }
}
pub(super) fn extract(s:&mut PdfiumSession,doc:u64,page:u64,obj:u64,bounds:Rect,crop:Option<Crop>,alpha:u32,rotation:i32,mirror:(bool,bool))->Result<Option<ImageBlock>,PdfiumError> {
    let scratch=s.alloc(8)?;
    let size=(|| { if number(s,"FPDFImageObj_GetImagePixelSize",&[ptr(obj,0),ptr(scratch,0),ptr(scratch,4)])?==0.0 { return Ok((0,0)); } let b=s.read(scratch,0,8)?; Ok((u32::from_le_bytes(b[0..4].try_into().expect("pixel width")),u32::from_le_bytes(b[4..8].try_into().expect("pixel height")))) })();
    let free=s.free(scratch); let (w,h)=match (size,free) { (Err(e),_) | (Ok(_),Err(e))=>return Err(e), (Ok(v),Ok(()))=>v };
    let mut px=rendered(s,doc,page,obj,1.0)?;
    let transparent=px.as_ref().is_some_and(|p|p.pixels.iter().skip(3).step_by(4).any(|a|*a<250));
    let needs_surgery=crop.is_some() || alpha<250 || rotation!=0 || mirror.0 || mirror.1;
    if !transparent && !needs_surgery && w>0 && h>0 && filters(s,obj)?==["DCTDecode"] {
        if let Some(data)=raw_jpeg(s,obj)? { return Ok(Some(ImageBlock { bounds,data,mime:"image/jpeg".into(),pixel_width:w,pixel_height:h,float:None,spacing_before_pt:None,z:None,synthetic:false,card_id:None })); }
    }
    let Some(mut image)=px.take() else { return Ok(None) };
    let gain=(w as f64/(image.width.max(1) as f64)).max(h as f64/(image.height.max(1) as f64));
    if gain>1.3 { let scale=gain.min(4096.0/image.width.max(1) as f64).min(4096.0/image.height.max(1) as f64); if scale>1.0 { if let Some(high)=rendered(s,doc,page,obj,scale)? { image=high; } } }
    if mirror.0 || mirror.1 { image=bitmap::mirror(image,mirror.0,mirror.1); }
    if let Some(c)=crop { let Some(cropped)=bitmap::crop(&image,c.left,c.top,c.right,c.bottom) else { return Ok(None) }; image=cropped; }
    if alpha<250 { for a in image.pixels.iter_mut().skip(3).step_by(4) { *a=((u32::from(*a)*alpha+127)>>8) as u8; } }
    if rotation!=0 { image=bitmap::rotate(image,rotation); }
    let data=bitmap::encode_png(&image).map_err(PdfiumError::from)?;
    Ok(Some(ImageBlock { bounds,data,mime:"image/png".into(),pixel_width:image.width as u32,pixel_height:image.height as u32,float:None,spacing_before_pt:None,z:None,synthetic:false,card_id:None }))
}
