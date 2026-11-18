//! Request-scoped PDFium primitives for writeback reading and editing.
use std::collections::HashMap;
use serde_json::{json, Value};
use super::super::super::super::pdfium_session::{PdfiumSession, read_number, wire_number};
use super::analyze::{self, FormChar, TextObject};
use super::frame::PageFrame;
use super::layout::Rect;

fn reference(id: u64) -> Value { json!({"ref":id}) }
fn offset(id: u64, at: usize) -> Value { json!({"ref":id,"offset":at}) }
fn number(value: Value) -> Result<f64, String> { read_number(&value).ok_or_else(|| "invalid PDFium numeric result".into()) }
fn integer(value: Value) -> Result<i64, String> { value.as_i64().ok_or_else(|| "invalid PDFium integer result".into()) }
fn handle(value: Value) -> Result<Option<u64>, String> {
    if value.as_u64() == Some(0) { return Ok(None); }
    value.get("ref").and_then(Value::as_u64).filter(|id| *id != 0).map(Some).ok_or_else(|| "invalid PDFium handle result".into())
}
fn f32_at(bytes: &[u8], at: usize) -> Result<f64, String> {
    let raw: [u8; 4] = bytes.get(at..at+4).ok_or("invalid PDFium float buffer")?.try_into().map_err(|_| "invalid PDFium float buffer")?;
    Ok(f64::from(f32::from_le_bytes(raw)))
}
fn f64_at(bytes: &[u8], at: usize) -> Result<f64, String> {
    let raw: [u8; 8] = bytes.get(at..at+8).ok_or("invalid PDFium double buffer")?.try_into().map_err(|_| "invalid PDFium double buffer")?;
    Ok(f64::from_le_bytes(raw))
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32, String> {
    let raw: [u8; 4] = bytes.get(at..at+4).ok_or("invalid PDFium integer buffer")?.try_into().map_err(|_| "invalid PDFium integer buffer")?;
    Ok(u32::from_le_bytes(raw))
}
fn with_buffer<T>(session: &mut PdfiumSession, size: usize, f: impl FnOnce(&mut PdfiumSession, u64) -> Result<T, String>) -> Result<T, String> {
    let buffer = session.alloc(size).map_err(|e| e.to_string())?;
    let result = f(session, buffer);
    let released = session.free(buffer).map_err(|e| e.to_string());
    match result { Err(error) => Err(error), Ok(value) => { released?; Ok(value) } }
}
fn call(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<Value, String> { session.call(name, args).map_err(|e| e.to_string()) }
fn successful(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<bool, String> { Ok(integer(call(session, name, args)?)? != 0) }
fn rect_at(session: &mut PdfiumSession, name: &str, object: u64) -> Result<Option<Rect>, String> {
    with_buffer(session, 16, |session, buf| {
        if !successful(session, name, &[reference(object), offset(buf,0), offset(buf,4), offset(buf,8), offset(buf,12)])? { return Ok(None); }
        let bytes = session.read(buf, 0, 16).map_err(|e| e.to_string())?;
        Ok(Some(Rect { x0:f32_at(&bytes,0)?, y0:f32_at(&bytes,4)?, x1:f32_at(&bytes,8)?, y1:f32_at(&bytes,12)? }))
    })
}
pub(super) fn frame(session: &mut PdfiumSession, page: u64) -> Result<PageFrame, String> {
    let width = number(call(session, "FPDF_GetPageWidthF", &[reference(page)])?)?;
    let height = number(call(session, "FPDF_GetPageHeightF", &[reference(page)])?)?;
    let rotation = integer(call(session, "FPDFPage_GetRotation", &[reference(page)])?)?;
    let origin = match rect_at(session, "FPDFPage_GetCropBox", page)? {
        Some(origin) => Some(origin),
        None => rect_at(session, "FPDFPage_GetMediaBox", page)?,
    };
    Ok(PageFrame::new(width, height, i32::try_from(rotation).map_err(|_| "invalid page rotation")?, origin))
}
fn fill_color(session: &mut PdfiumSession, object: u64) -> Result<Option<[u8;3]>, String> {
    with_buffer(session, 16, |session, buf| {
        if !successful(session, "FPDFPageObj_GetFillColor", &[reference(object), offset(buf,0), offset(buf,4), offset(buf,8), offset(buf,12)])? { return Ok(None); }
        let bytes = session.read(buf,0,16).map_err(|e| e.to_string())?;
        Ok(Some([u8::try_from(u32_at(&bytes,0)?).map_err(|_| "invalid PDF color")?, u8::try_from(u32_at(&bytes,4)?).map_err(|_| "invalid PDF color")?, u8::try_from(u32_at(&bytes,8)?).map_err(|_| "invalid PDF color")?]))
    })
}
struct Accumulator { handle:u64, chars:String, sizes:Vec<f64>, origins:Vec<(f64,f64)>, turned:usize }
pub(super) struct PageObjects { pub text:Vec<TextObject>, pub form_chars:Vec<FormChar>, pub shapes:Vec<Rect> }

pub(super) fn read_page(session: &mut PdfiumSession, page: u64, frame: PageFrame) -> Result<PageObjects, String> {
    let count = integer(call(session,"FPDFPage_CountObjects",&[reference(page)])?)?;
    let mut objects = HashMap::new();
    let mut shapes = Vec::new();
    for index in 0..count {
        let Some(object) = handle(call(session,"FPDFPage_GetObject",&[reference(page), json!(index)])?)? else { continue };
        let kind = integer(call(session,"FPDFPageObj_GetType",&[reference(object)])?)?;
        if kind == 1 { objects.insert(object, usize::try_from(index).map_err(|_| "invalid object index")?); }
        else if let Some(bounds) = rect_at(session,"FPDFPageObj_GetBounds",object)? { shapes.push(frame.rect_to_display(bounds)); }
    }
    let Some(text_page) = handle(call(session,"FPDFText_LoadPage",&[reference(page)])?)? else { return Ok(PageObjects { text:Vec::new(), form_chars:Vec::new(), shapes }); };
    let result = (|| {
        let count = integer(call(session,"FPDFText_CountChars",&[reference(text_page)])?)?;
        let mut accumulators: HashMap<usize, Accumulator> = HashMap::new();
        let mut form_chars = Vec::new();
        with_buffer(session, 40, |session, scratch| {
            for index in 0..count {
                let Some(object) = handle(call(session,"FPDFText_GetTextObject",&[reference(text_page),json!(index)])?)? else { continue };
                let cp = integer(call(session,"FPDFText_GetUnicode",&[reference(text_page),json!(index)])?)?;
                let ink = cp > 0x20 || cp == 2;
                call(session,"FPDFText_GetCharOrigin",&[reference(text_page),json!(index),offset(scratch,0),offset(scratch,8)])?;
                let bytes = session.read(scratch,0,16).map_err(|e|e.to_string())?;
                let origin = frame.to_display(f64_at(&bytes,0)?, f64_at(&bytes,8)?);
                let size = if ink {
                    let nominal = number(call(session,"FPDFText_GetFontSize",&[reference(text_page),json!(index)])?)?;
                    let has_matrix = successful(session,"FPDFText_GetMatrix",&[reference(text_page),json!(index),offset(scratch,16)])?;
                    let scale = if has_matrix { let matrix = session.read(scratch,16,24).map_err(|e|e.to_string())?; f32_at(&matrix,8)?.hypot(f32_at(&matrix,12)?) } else { 1.0 };
                    nominal * if scale > 0.0 { scale } else { 1.0 }
                } else { 0.0 };
                let Some(object_index) = objects.get(&object).copied() else { if ink { form_chars.push(FormChar { at:origin, size }); } continue };
                let acc = accumulators.entry(object_index).or_insert_with(|| Accumulator { handle:object, chars:String::new(), sizes:Vec::new(), origins:Vec::new(), turned:0 });
                let ch = if cp == 2 { '-' } else { char::from_u32(u32::try_from(cp).map_err(|_| "invalid PDF character")?).ok_or("invalid PDF character")? };
                acc.chars.push(ch);
                if ink {
                    acc.sizes.push(size); acc.origins.push(origin);
                    let angle = number(call(session,"FPDFText_GetCharAngle",&[reference(text_page),json!(index)])?)? + frame.angle_delta();
                    let angle = angle - 2.0 * std::f64::consts::PI * jscompat::math_round(angle / (2.0 * std::f64::consts::PI));
                    if angle.abs() > 0.15 { acc.turned += 1; }
                }
            }
            Ok(())
        })?;
        let mut text = Vec::new();
        for (index, acc) in accumulators {
            if acc.origins.is_empty() { continue; }
            let Some(bounds) = rect_at(session,"FPDFPageObj_GetBounds",acc.handle)? else { continue };
            let size = match analyze::median(&acc.sizes) { x if x != 0.0 => x, _ => 1.0 };
            let first = acc.origins[0]; let last = acc.origins[acc.origins.len()-1];
            let dx = (last.0-first.0).abs(); let dy = (last.1-first.1).abs();
            let mode = integer(call(session,"FPDFTextObj_GetTextRenderMode",&[reference(acc.handle)])?)?;
            text.push(TextObject { index, bounds:frame.rect_to_display(bounds), text:acc.chars, size, baselines:acc.origins.iter().map(|p|p.1).collect(), color:fill_color(session,acc.handle)?.unwrap_or([0,0,0]), invisible:matches!(mode,3|7), vertical:acc.turned*2>acc.origins.len() || (dy>dx && dy>size*0.5) });
        }
        text.sort_by_key(|item|item.index);
        Ok(PageObjects { text, form_chars, shapes })
    })();
    let closed = call(session,"FPDFText_ClosePage",&[reference(text_page)]);
    match result { Err(e) => Err(e), Ok(result) => { closed?; Ok(result) } }
}

pub(super) fn page_count(session: &mut PdfiumSession) -> Result<usize, String> {
    let document = session.document();
    let count = integer(call(session, "FPDF_GetPageCount", &[reference(document)])?)?;
    usize::try_from(count).map_err(|_| "invalid PDF page count".into())
}
pub(super) fn encrypted(session: &mut PdfiumSession) -> Result<bool, String> {
    let document = session.document();
    Ok(integer(call(session, "FPDF_GetSecurityHandlerRevision", &[reference(document)])?)? != -1)
}
pub(super) fn load_page(session: &mut PdfiumSession, page: usize) -> Result<u64, String> {
    let document = session.document();
    let index = page.checked_sub(1).ok_or("invalid PDF page number")?;
    handle(call(session, "FPDF_LoadPage", &[reference(document), json!(index)])?)?
        .ok_or_else(|| format!("could not load page {page}"))
}
pub(super) fn close_page(session: &mut PdfiumSession, page: u64) -> Result<(), String> {
    call(session, "FPDF_ClosePage", &[reference(page)])?;
    Ok(())
}
pub(super) struct DrawLine { pub text:String, pub x:f64, pub y:f64, pub size:f64, pub color:[u8;3], pub bold:bool, pub face:usize }
#[derive(Default)]
pub(super) struct PageOps { pub remove:std::collections::BTreeSet<usize>, pub paints:Vec<(Rect,[u8;3])>, pub lines:Vec<DrawLine> }

pub(super) fn load_font(session: &mut PdfiumSession, bytes: &[u8]) -> Result<u64, String> {
    let document = session.document();
    with_buffer(session, bytes.len(), |session, buffer| {
        session.write(buffer, 0, bytes).map_err(|e| e.to_string())?;
        handle(call(session, "FPDFText_LoadFont", &[reference(document), reference(buffer), json!(bytes.len()), json!(2), json!(1)])?)?
            .ok_or_else(|| "PDFium could not load the embedded font".into())
    })
}

pub(super) fn write_page(session: &mut PdfiumSession, page_number: usize, ops: &PageOps, fonts: &[u64]) -> Result<(), String> {
    let page = load_page(session, page_number)?;
    let result = (|| {
        let frame = frame(session, page)?;
        let count = integer(call(session, "FPDFPage_CountObjects", &[reference(page)])?)?;
        let mut objects = Vec::new();
        for index in 0..count { objects.push(handle(call(session, "FPDFPage_GetObject", &[reference(page), json!(index)])?)?); }
        for index in &ops.remove {
            let Some(Some(object)) = objects.get(*index) else { continue };
            if successful(session, "FPDFPage_RemoveObject", &[reference(page), reference(*object)])? {
                call(session, "FPDFPageObj_Destroy", &[reference(*object)])?;
            }
        }
        for (rect, color) in &ops.paints {
            let rect = frame.rect_to_user(*rect);
            let Some(path) = handle(call(session, "FPDFPageObj_CreateNewRect", &[wire_number(rect.x0), wire_number(rect.y0), wire_number(rect.x1-rect.x0), wire_number(rect.y1-rect.y0)])?)? else { continue };
            call(session, "FPDFPageObj_SetFillColor", &[reference(path), json!(color[0]), json!(color[1]), json!(color[2]), json!(255)])?;
            call(session, "FPDFPath_SetDrawMode", &[reference(path), json!(1), json!(0)])?;
            call(session, "FPDFPage_InsertObject", &[reference(page), reference(path)])?;
        }
        let document = session.document();
        let axes = frame.text_axes();
        with_buffer(session, 24, |session, matrix| {
            for line in &ops.lines {
                let font = *fonts.get(line.face).ok_or("translated line has no font")?;
                let object = handle(call(session, "FPDFPageObj_CreateTextObj", &[reference(document),reference(font),wire_number(line.size)])?)?.ok_or("PDFium could not create a text object")?;
                let mut text = Vec::with_capacity((line.text.encode_utf16().count()+1)*2);
                for unit in line.text.encode_utf16().chain(std::iter::once(0)) { text.extend_from_slice(&unit.to_le_bytes()); }
                let set = with_buffer(session, text.len(), |session, buffer| {
                    session.write(buffer,0,&text).map_err(|e|e.to_string())?;
                    successful(session, "FPDFText_SetText", &[reference(object), reference(buffer)])
                });
                if !matches!(&set, Ok(true)) {
                    let _ = call(session,"FPDFPageObj_Destroy",&[reference(object)]);
                    return match set { Err(error) => Err(error), _ => Err("PDFium could not set the translated text".into()) };
                }
                let (x,y) = frame.to_user(line.x,line.y);
                let mut bytes = Vec::with_capacity(24);
                for value in [axes[0],axes[1],axes[2],axes[3],x,y] { bytes.extend_from_slice(&(value as f32).to_le_bytes()); }
                session.write(matrix,0,&bytes).map_err(|e|e.to_string())?;
                call(session,"FPDFPageObj_SetMatrix",&[reference(object),reference(matrix)])?;
                call(session,"FPDFPageObj_SetFillColor",&[reference(object),json!(line.color[0]),json!(line.color[1]),json!(line.color[2]),json!(255)])?;
                if line.bold {
                    call(session,"FPDFTextObj_SetTextRenderMode",&[reference(object),json!(2)])?;
                    call(session,"FPDFPageObj_SetStrokeColor",&[reference(object),json!(line.color[0]),json!(line.color[1]),json!(line.color[2]),json!(255)])?;
                    call(session,"FPDFPageObj_SetStrokeWidth",&[reference(object),wire_number(line.size*0.03)])?;
                }
                call(session,"FPDFPage_InsertObject",&[reference(page),reference(object)])?;
            }
            Ok(())
        })?;
        if !successful(session,"FPDFPage_GenerateContent",&[reference(page)])? { return Err(format!("could not rewrite page {page_number}")); }
        Ok(())
    })();
    let closed = close_page(session,page);
    match result { Err(error) => Err(error), Ok(()) => closed }
}
pub(super) fn save(session: &mut PdfiumSession) -> Result<Vec<u8>, String> {
    let document = session.document();
    save_document(session, document)
}
fn save_document(session: &mut PdfiumSession, document: u64) -> Result<Vec<u8>, String> {
    let writer = handle(call(session,"PDFiumExt_OpenFileWriter",&[])?)?.ok_or("PDFium could not create a file writer")?;
    let result = (|| {
        if !successful(session,"PDFiumExt_SaveAsCopy",&[reference(document),reference(writer)])? { return Err("PDFium could not save the document".into()); }
        let size = usize::try_from(integer(call(session,"PDFiumExt_GetFileWriterSize",&[reference(writer)])?)?).map_err(|_| "invalid PDF writer size")?;
        with_buffer(session,size,|session,buffer| {
            call(session,"PDFiumExt_GetFileWriterData",&[reference(writer),reference(buffer),json!(size)])?;
            session.read(buffer,0,size).map_err(|e|e.to_string())
        })
    })();
    let closed = call(session,"PDFiumExt_CloseFileWriter",&[reference(writer)]);
    match result { Err(error)=>Err(error), Ok(bytes)=>{closed?;Ok(bytes)} }
}

fn shown_size(session: &mut PdfiumSession, document: u64, index: usize) -> Result<(f64,f64),String> {
    let page = handle(call(session,"FPDF_LoadPage",&[reference(document),json!(index)])?)?.ok_or_else(||format!("could not load page {}",index+1))?;
    let result = frame(session,page).map(|frame|(frame.width,frame.height));
    let closed = close_page(session,page);
    match result { Err(error)=>Err(error), Ok(size)=>{closed?;Ok(size)} }
}
fn place_xobject(session:&mut PdfiumSession, dest:u64, sheet:u64, source:u64, index:usize, dx:f64, dy:f64)->Result<(),String> {
    let object = handle(call(session,"FPDF_NewXObjectFromPage",&[reference(dest),reference(source),json!(index)])?)?.ok_or_else(||format!("could not take page {}",index+1))?;
    let result = (|| {
        let form = handle(call(session,"FPDF_NewFormObjectFromXObject",&[reference(object)])?)?.ok_or_else(||format!("could not place page {}",index+1))?;
        call(session,"FPDFPageObj_Transform",&[reference(form),json!(1),json!(0),json!(0),json!(1),wire_number(dx),wire_number(dy)])?;
        call(session,"FPDFPage_InsertObject",&[reference(sheet),reference(form)])?;
        Ok(())
    })();
    let closed=call(session,"FPDF_CloseXObject",&[reference(object)]);
    match result {Err(error)=>Err(error),Ok(())=>{closed?;Ok(())}}
}
pub(super) fn bilingual(session:&mut PdfiumSession, original:&[u8])->Result<Vec<u8>,String> {
    let translated = session.document();
    with_buffer(session,original.len(),|session,buffer| {
        session.write(buffer,0,original).map_err(|e|e.to_string())?;
        let source=handle(call(session,"FPDF_LoadMemDocument",&[reference(buffer),json!(original.len()),json!(0)])?)?.ok_or("could not reopen original PDF")?;
        let result=(|| {
            let left=integer(call(session,"FPDF_GetPageCount",&[reference(source)])?)?;
            let right=integer(call(session,"FPDF_GetPageCount",&[reference(translated)])?)?;
            let count=usize::try_from(left.min(right)).map_err(|_|"invalid PDF page count")?;
            let output=handle(call(session,"FPDF_CreateNewDocument",&[])?)?.ok_or("PDFium could not create a document")?;
            let output_result=(|| {
                for index in 0..count {
                    let left=shown_size(session,source,index)?;
                    let right=shown_size(session,translated,index)?;
                    let height=left.1.max(right.1);
                    let sheet=handle(call(session,"FPDFPage_New",&[reference(output),json!(index),wire_number(left.0+right.0),wire_number(height)])?)?.ok_or_else(||format!("could not add sheet {}",index+1))?;
                    let written=(|| {
                        place_xobject(session,output,sheet,source,index,0.0,height-left.1)?;
                        place_xobject(session,output,sheet,translated,index,left.0,height-right.1)?;
                        if !successful(session,"FPDFPage_GenerateContent",&[reference(sheet)])? {return Err(format!("could not write sheet {}",index+1));}
                        Ok(())
                    })();
                    let closed=close_page(session,sheet);
                    match written {Err(error)=>return Err(error),Ok(())=>closed?}
                }
                save_document(session,output)
            })();
            let closed=call(session,"FPDF_CloseDocument",&[reference(output)]);
            match output_result {Err(error)=>Err(error),Ok(bytes)=>{closed?;Ok(bytes)}}
        })();
        let closed=call(session,"FPDF_CloseDocument",&[reference(source)]);
        match result {Err(error)=>Err(error),Ok(bytes)=>{closed?;Ok(bytes)}}
    })
}

pub(super) fn render_bitmap(session: &mut PdfiumSession, page: u64, frame: PageFrame, scale: f64) -> Result<Option<super::bitmap::PageBitmap>, String> {
    let width = jscompat::math_round(frame.width * scale).max(1.0);
    let height = jscompat::math_round(frame.height * scale).max(1.0);
    if !width.is_finite() || !height.is_finite() || width > f64::from(i32::MAX) || height > f64::from(i32::MAX) { return Err("invalid PDF bitmap dimensions".into()); }
    let width = width as usize;
    let height = height as usize;
    let Some(bitmap) = handle(call(session,"FPDFBitmap_Create",&[json!(width),json!(height),json!(0)])?)? else { return Ok(None) };
    let result = (|| {
        call(session,"FPDFBitmap_FillRect",&[reference(bitmap),json!(0),json!(0),json!(width),json!(height),json!(0xffff_ffffu32)])?;
        call(session,"FPDF_RenderPageBitmap",&[reference(bitmap),reference(page),json!(0),json!(0),json!(width),json!(height),json!(0),json!(0)])?;
        let stride = usize::try_from(integer(call(session,"FPDFBitmap_GetStride",&[reference(bitmap)])?)?).map_err(|_| "invalid PDF bitmap stride")?;
        let format = integer(call(session,"FPDFBitmap_GetFormat",&[reference(bitmap)])?)?;
        let Some(buffer) = handle(call(session,"FPDFBitmap_GetBuffer",&[reference(bitmap)])?)? else { return Ok(None) };
        let bpp = match format { 3|4=>4usize, 2=>3, 1=>1, _=>return Ok(None) };
        let input_len = stride.checked_mul(height).ok_or("PDF bitmap too large")?;
        let output_len = width.checked_mul(height).and_then(|n|n.checked_mul(4)).ok_or("PDF bitmap too large")?;
        if stride < width.checked_mul(bpp).ok_or("PDF bitmap too large")? { return Err("invalid PDF bitmap stride".into()); }
        let input = session.read(buffer,0,input_len).map_err(|e|e.to_string())?;
        let mut rgba = vec![0; output_len];
        for y in 0..height { for x in 0..width {
            let src = y*stride+x*bpp; let dst = (y*width+x)*4;
            if bpp==1 { rgba[dst]=input[src]; rgba[dst+1]=input[src]; rgba[dst+2]=input[src]; }
            else { rgba[dst]=input[src+2]; rgba[dst+1]=input[src+1]; rgba[dst+2]=input[src]; }
            rgba[dst+3]=255;
        }}
        Ok(Some(super::bitmap::PageBitmap { rgba,width,height,scale,height_pt:frame.height }))
    })();
    let closed = call(session,"FPDFBitmap_Destroy",&[reference(bitmap)]);
    match result { Err(error)=>Err(error), Ok(value)=>{closed?;Ok(value)} }
}
