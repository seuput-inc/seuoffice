//! Document-font preservation and rebuild-face resolution.
use std::collections::HashSet;
use serde_json::json;
use shell_contract::{IpcValue, runtime::CancelToken};
use main_core::pdf::{font_cmap::font_covers_text, font_classes::decode_font_name, font_locate::{FontEnv,FontLocator,Platform,is_truetype}, font_similar::{similar_font_for,SourceFace}, font_subset::identity_cff_charset};
use super::super::{PdfiumSession,buffer,call,field,handle,integer,reference,text};
use super::super::font::{chosen,fallback,subset};

#[derive(Clone)]
pub(super) struct Face { pub handle: u64, pub synthetic: bool }
pub(super) struct OwnFace { pub face: Face, pub covered: HashSet<char> }
fn flag(edit:&IpcValue,key:&str)->bool { matches!(field(edit,key),Some(IpcValue::Bool(true))) }

fn name(session:&mut PdfiumSession,font:u64,method:&str)->Result<String,String> {
    let len=usize::try_from(integer(call(session,method,&[reference(font),json!(0),json!(0)])?)?).map_err(|_|"invalid font name length")?;
    if len<=1 { return Ok(String::new()) }
    buffer(session,len,|s,b| { call(s,method,&[reference(font),reference(b),json!(len)])?; let bytes=s.read(b,0,len-1).map_err(|e|e.to_string())?; Ok(decode_font_name(&bytes)) })
}
fn embedded(session:&mut PdfiumSession,font:u64)->Result<Option<Vec<u8>>,String> {
    if integer(call(session,"FPDFFont_GetIsEmbedded",&[reference(font)])?)?==0 { return Ok(None) }
    buffer(session,4,|s,lenptr| {
        if integer(call(s,"FPDFFont_GetFontData",&[reference(font),json!(0),json!(0),reference(lenptr)])?)?==0 {return Ok(None)}
        let n=s.read(lenptr,0,4).map_err(|e|e.to_string())?;
        let len=u32::from_le_bytes(n.as_slice().try_into().map_err(|_|"invalid font length")?) as usize;
        if len==0 {return Ok(None)}
        buffer(s,len,|s,data| {
            if integer(call(s,"FPDFFont_GetFontData",&[reference(font),reference(data),json!(len),reference(lenptr)])?)?==0{return Ok(None)}
            Ok(Some(s.read(data,0,len).map_err(|e|e.to_string())?))
        })
    })
}
fn bold(name:&str)->bool { let name=name.to_lowercase(); ["bold","black","heavy","semibold","demibold","extrabold","ultrabold"].iter().any(|s|name.contains(s)) }
fn clean_ps(ps:&str)->&str { let b=ps.as_bytes(); if b.len()>=7 && b[..6].iter().all(u8::is_ascii_uppercase) && b[6]==b'+' { &ps[7..] } else { ps } }
fn strip_style_tokens(ps:&str)->String {
    let lower=ps.to_ascii_lowercase();
    let mut result=String::new();
    let mut pos=0;
    while pos<ps.len() {
        if let Some(token)=["bolditalic","bold","italic","oblique"].into_iter().find(|token|lower[pos..].starts_with(token)) {
            pos+=token.len();
        } else {
            let ch=ps[pos..].chars().next().expect("character boundary");
            result.push(ch);pos+=ch.len_utf8();
        }
    }
    result
}
fn standard(name:&str,drawn:&str)->bool {
    const NAMES:[&str;12]=["Helvetica","Helvetica-Bold","Helvetica-Oblique","Helvetica-BoldOblique","Times-Roman","Times-Bold","Times-Italic","Times-BoldItalic","Courier","Courier-Bold","Courier-Oblique","Courier-BoldOblique"];
    NAMES.contains(&name) && drawn.chars().all(|c| (' '..='~').contains(&c) || ('\u{a0}'..='\u{ff}').contains(&c) || "€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ".contains(c))
}
fn locator()->FontLocator { FontLocator::new(FontEnv {platform:if cfg!(target_os="macos"){Platform::Darwin}else if cfg!(windows){Platform::Win32}else{Platform::Other},home:dirs::home_dir().unwrap_or_default().to_string_lossy().into_owned()}) }
fn load_bytes(s:&mut PdfiumSession,doc:u64,bytes:&[u8],any_cff:&mut bool)->Result<u64,String>{
    let truetype=is_truetype(bytes);
    let font=buffer(s,bytes.len(),|s,b|{ s.write(b,0,bytes).map_err(|e|e.to_string())?; handle(call(s,"FPDFText_LoadFont",&[reference(doc),reference(b),json!(bytes.len()),json!(if truetype{2}else{1}),json!(1)])?)?.ok_or("FPDFText_LoadFont failed".into()) })?;
    *any_cff |= !truetype;
    Ok(font)
}

pub(super) fn load(s:&mut PdfiumSession,doc:u64,anchor:Option<u64>,edit:&IpcValue,drawn:&str,cancel:&CancelToken,any_cff:&mut bool)->Result<Face,String>{
    if cancel.is_cancelled(){return Err("cancelled".into())}
    let chosen_name=text(field(edit,"newFont")).filter(|s|!s.is_empty());
    let italic=flag(edit,"newItalic");
    let requested_bold=flag(edit,"newBold");
    let drawn=if drawn.trim().is_empty(){"x".to_owned()}else{drawn.to_owned()}.replace('\n',"");
    let ps=if let Some(font)=anchor {name(s,font,"FPDFFont_GetBaseFontName")?}else{String::new()};
    let embedded_flag=if let Some(font)=anchor { integer(call(s,"FPDFFont_GetIsEmbedded",&[reference(font)])?)?!=0 } else {false};
    if !embedded_flag && chosen_name.is_none() && !italic && standard(&ps,&drawn) {
        let mut bytes=ps.as_bytes().to_vec();bytes.push(0);
        let handle=buffer(s,bytes.len(),|s,b|{s.write(b,0,&bytes).map_err(|e|e.to_string())?;handle(call(s,"FPDFText_LoadStandardFont",&[reference(doc),reference(b)])?)?.ok_or("FPDFText_LoadStandardFont failed".into())})?;
        return Ok(Face{handle,synthetic:requested_bold&&!bold(&ps)})
    }
    let want_bold=requested_bold&&(chosen_name.is_some()||!bold(&ps));
    if let Some(id)=chosen_name {
        let variants=if want_bold||italic {vec![(want_bold,italic),(false,false)]}else{vec![(false,false)]};
        for (b,i) in variants {if let Some(bytes)=chosen(id,b,i).filter(|bytes|font_covers_text(bytes,&drawn)) {let bytes=subset(&bytes,&drawn,cancel)?;return Ok(Face{handle:load_bytes(s,doc,&bytes,any_cff)?,synthetic:want_bold&&!b})}}
    } else if let Some(font)=anchor {
        let family=name(s,font,"FPDFFont_GetFamilyName")?;
        let ps=clean_ps(&ps);
        let mut locator=locator();
        if italic {
            let style="italic";
            let mut system=locator.find_system_font(&format!("{ps}-{style}"),&family);
            let original=locator.find_system_font(ps,&family);
            let already=ps.to_lowercase().contains("italic")||ps.to_lowercase().contains("oblique");
            if !already && system.is_some() && system==original {
                let stripped=strip_style_tokens(ps);
                let alt=locator.find_system_font(&format!("{stripped}-{style}"),&family);
                system=alt.filter(|v|Some(v)!=original.as_ref());
            }
            if let Some(bytes)=system.filter(|v|font_covers_text(v,&drawn)) {
                if let Ok(sub)=subset(&bytes,&drawn,cancel) { if let Ok(sub)=identity_cff_charset(&sub) {return Ok(Face{handle:load_bytes(s,doc,&sub,any_cff)?,synthetic:want_bold})} }
                if cancel.is_cancelled(){return Err("cancelled".into())}
            }
        }
        let source=embedded(s,font)?;
        if let Some(bytes)=source.as_ref().filter(|v|font_covers_text(v,&drawn)) {
            if let Ok(bytes)=identity_cff_charset(bytes) {return Ok(Face{handle:load_bytes(s,doc,&bytes,any_cff)?,synthetic:want_bold})}
        }
        if let Some(bytes)=locator.find_system_font(ps,&family).filter(|v|font_covers_text(v,&drawn)) {
            if let Ok(sub)=subset(&bytes,&drawn,cancel) {if let Ok(sub)=identity_cff_charset(&sub){return Ok(Face{handle:load_bytes(s,doc,&sub,any_cff)?,synthetic:want_bold})}}
            if cancel.is_cancelled(){return Err("cancelled".into())}
        }
        let flags=integer(call(s,"FPDFFont_GetFlags",&[reference(font)])?)? as f64;
        if let Some(similar)=similar_font_for(&mut locator,&SourceFace{ps_name:ps.to_owned(),flags,embedded:source},&drawn,italic) {
            if let Ok(sub)=subset(&similar.bytes,&drawn,cancel) {if let Ok(sub)=identity_cff_charset(&sub){return Ok(Face{handle:load_bytes(s,doc,&sub,any_cff)?,synthetic:(want_bold||similar.source_bold)&&!similar.bold})}}
            if cancel.is_cancelled(){return Err("cancelled".into())}
        }
    }
    let bytes=fallback(&drawn).ok_or("the replacement contains characters no available font can draw")?;
    let sub=subset(&bytes,&drawn,cancel)?;
    let bytes=match identity_cff_charset(&sub) {Ok(bytes)=>bytes,Err(_) if is_truetype(&sub)=>sub,Err(_)=>return Err("the fallback font for this text could not be embedded".into())};
    Ok(Face{handle:load_bytes(s,doc,&bytes,any_cff)?,synthetic:want_bold})
}

pub(super) fn partial(s:&mut PdfiumSession,doc:u64,anchor:Option<u64>,drawn:&str,any_cff:&mut bool)->Result<Option<OwnFace>,String>{
    let Some(font)=anchor else{return Ok(None)};
    let Some(bytes)=embedded(s,font)? else{return Ok(None)};
    let mut covered=HashSet::new();let mut missing=false;let mut visible=false;
    for ch in drawn.chars().filter(|ch|!matches!(ch,'\r'|'\n')) {
        if font_covers_text(&bytes,&ch.to_string()) {covered.insert(ch);visible|=!jscompat::is_js_space(ch)} else {missing=true}
    }
    if !missing||!visible{return Ok(None)}
    let Ok(bytes)=identity_cff_charset(&bytes) else{return Ok(None)};
    Ok(Some(OwnFace{face:Face{handle:load_bytes(s,doc,&bytes,any_cff)?,synthetic:false},covered}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_font_is_only_retained_for_win_ansi() {
        assert!(standard("Helvetica", "A€"));
        assert!(!standard("Helvetica", "한"));
        assert!(!standard("Symbol", "A"));
    }
    #[test]
    fn source_style_removal_is_ascii_case_insensitive() {
        assert_eq!(strip_style_tokens("한글-bOlDiTaLiC"), "한글-");
        assert_eq!(clean_ps("ABCDEF+Helvetica"), "Helvetica");
    }
}
