//! PDF Info dictionary values used by document classification.
use serde_json::json;
use crate::host::d2::pdf::pdfium_session::{PdfiumError,PdfiumSession};
use super::paths::{number,ptr};

pub(super) fn read(s:&mut PdfiumSession,doc:u64,tag:&str)->Result<Option<String>,PdfiumError> {
    let key=s.alloc(tag.len()+1)?;
    let result=(|| {
        let mut name=tag.as_bytes().to_vec(); name.push(0); s.write(key,0,&name)?;
        let needed=number(s,"FPDF_GetMetaText",&[ptr(doc,0),ptr(key,0),json!(0),json!(0)])? as usize;
        if needed<=2 {return Ok(None);}
        let buffer=s.alloc(needed)?;
        let text=(|| {
            number(s,"FPDF_GetMetaText",&[ptr(doc,0),ptr(key,0),ptr(buffer,0),json!(needed)])?;
            let bytes=s.read(buffer,0,needed-2)?;
            let units:Vec<u16>=bytes.chunks_exact(2).map(|b|u16::from_le_bytes([b[0],b[1]])).collect();
            let value=String::from_utf16_lossy(&units); let value=jscompat::trim(&value);
            Ok((!value.is_empty()).then(||value.to_owned()))
        })();
        let freed=s.free(buffer); match (text,freed) {(Err(e),_) | (Ok(_),Err(e))=>Err(e),(Ok(v),Ok(()))=>Ok(v)}
    })();
    let freed=s.free(key); match (result,freed) {(Err(e),_) | (Ok(_),Err(e))=>Err(e),(Ok(v),Ok(()))=>Ok(v)}
}
