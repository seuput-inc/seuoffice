//! Convert accepted OCR recognition into analysis-layer glyphs.
use serde_json::Value;
use shell_contract::runtime::CancelToken;
use super::geometry::Rect;
use super::ir::{ExtractedPage,PdfChar};
use super::script::{script_of,east_asian};
fn rect(v:&Value)->Option<Rect>{Some(Rect{x0:v.get("x0")?.as_f64()?,y0:v.get("y0")?.as_f64()?,x1:v.get("x1")?.as_f64()?,y1:v.get("y1")?.as_f64()?})}
fn weight(c:char)->f64{if east_asian(script_of(c as u32)){1.0}else if c==' '{0.4}else if c.is_ascii_digit() || c.is_ascii_uppercase(){0.6}else if ".,:;!'\"|()[]{}".contains(c){0.3}else{0.5}}
fn synth(c:char,x0:f64,x1:f64,baseline:f64,size:f64,cjk:bool)->PdfChar{let code=c as u32;PdfChar{code,text:c.to_string(),bounds:Rect{x0,x1,y0:baseline-size*0.21,y1:baseline+size*0.72},loose_box:Rect{x0,x1,y0:baseline-size*0.25,y1:baseline+size*0.95},origin_x:x0,origin_y:baseline,angle:0.0,font_size:size,font_weight:400.0,font_family:(if cjk{"SimSun"}else{"Times New Roman"}).into(),italic:false,color:"000000".into(),hscale:None,invisible:false,is_generated:false,pdf_char_index:None,text_object_id:None,is_hyphen:false,script:script_of(code),highlight:None,underline:false,strike:false,note_ref:None,href:None}}
fn segment(out:&mut Vec<PdfChar>,glyphs:&[char],x0:f64,x1:f64,baseline:f64,size:f64,cjk:bool){let total:f64=glyphs.iter().map(|c|weight(*c)).sum();if total<=0.0 || x1<=x0{return;}let mut x=x0;for g in glyphs {let w=weight(*g)/total*(x1-x0);out.push(synth(if g.is_whitespace(){' '}else{*g},x,x+w,baseline,size,cjk));x+=w;}}
fn chars(lines:&[&Value],w:f64,h:f64)->Vec<PdfChar>{let mut out=Vec::new();for line in lines {let Some(r)=line.get("box").and_then(rect) else{continue};let Some(text)=line.get("text").and_then(Value::as_str) else{continue};let r=Rect{x0:r.x0*w,x1:r.x1*w,y0:r.y0*h,y1:r.y1*h};let lh=r.y1-r.y0;if lh<=0.0 || r.x1<=r.x0 || jscompat::trim(text).is_empty(){continue;}let glyphs:Vec<char>=text.chars().collect();let body:Vec<_>=glyphs.iter().filter(|c|!c.is_whitespace()).collect();let cjk=!body.is_empty() && body.iter().filter(|c|east_asian(script_of(***c as u32))).count() as f64/body.len() as f64>=0.5;let size=jscompat::math_round(lh*(if cjk{0.9}else{0.72})*2.0)/2.0;if size<=0.0{continue;}let baseline=r.y0+(lh-size*0.93)/2.0+size*0.21;
        let engine=line.get("chars").and_then(Value::as_array).filter(|a|a.len()==glyphs.len() && a.iter().zip(&glyphs).all(|(v,c)|c.is_whitespace() || v.get("box").and_then(rect).is_some_and(|b|b.x1>b.x0)));
        if let Some(boxes)=engine {let mut cursor=r.x0;let mut i=0;while i<glyphs.len(){if glyphs[i].is_whitespace(){i+=1;continue;}let mut j=i;while j<glyphs.len() && !glyphs[j].is_whitespace(){j+=1;}let Some(first)=boxes[i].get("box").and_then(rect) else{break};let Some(last)=boxes[j-1].get("box").and_then(rect) else{break};let x0=cursor.max(first.x0*w);let x1=(x0+size*0.2).max(last.x1*w);if i>0 && x0>cursor{out.push(synth(' ',cursor,x0,baseline,size,cjk));}segment(&mut out,&glyphs[i..j],x0,x1,baseline,size,cjk);cursor=x1;i=j;}}else{segment(&mut out,&glyphs,r.x0,r.x1,baseline,size,cjk);}
    }out}
pub(super) fn recover(page:&ExtractedPage,png:&[u8])->Option<(ExtractedPage,f64)>{let recognition=crate::host::d2::pdf::native_callbacks::recognize_page(png,page.width_pt,page.height_pt,&CancelToken::default()).ok()??;if recognition.get("paperShare").and_then(Value::as_f64).is_some_and(|v|v<0.45){return None;}let kept:Vec<_>=recognition.get("lines")?.as_array()?.iter().filter(|l|l.get("confidence").and_then(Value::as_f64).is_some_and(|v|v>=0.3) && l.get("text").and_then(Value::as_str).is_some_and(|t|!jscompat::trim(t).is_empty())).collect();if kept.is_empty(){return None;}let coverage:f64=kept.iter().filter_map(|l|l.get("box").and_then(rect)).map(|r|(r.x1-r.x0).max(0.0)*(r.y1-r.y0).max(0.0)).sum();if coverage<0.004{return None;}let confidence=kept.iter().map(|l|l.get("confidence").and_then(Value::as_f64).unwrap_or(0.0)).sum::<f64>()/kept.len() as f64;if confidence<0.55{return None;}let extracted=chars(&kept,page.width_pt,page.height_pt);if extracted.iter().filter(|c|!jscompat::trim(&c.text).is_empty()).count()<8{return None;}let mut synthetic=page.clone();synthetic.chars=extracted;synthetic.images.clear();synthetic.paths.clear();synthetic.scanned=false;synthetic.degraded=false;synthetic.degraded_reason=None;synthetic.render=None;synthetic.background_render=None;synthetic.text_lost=false;synthetic.bad_unicode_ratio=0.0;Some((synthetic,confidence))}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlapping_engine_glyph_boxes_remain_non_overlapping() {
        let line=serde_json::json!({"text":"AB","confidence":0.9,"box":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":0.1},"chars":[{"text":"A","box":{"x0":0.0,"y0":0.0,"x1":0.8,"y1":0.1}},{"text":"B","box":{"x0":0.2,"y0":0.0,"x1":1.0,"y1":0.1}}]});
        let glyphs=chars(&[&line],100.0,100.0);
        assert_eq!(glyphs.len(),2);
        assert!(glyphs[0].bounds.x1<=glyphs[1].bounds.x0);
        assert_eq!(glyphs.iter().map(|c|c.text.as_str()).collect::<String>(),"AB");
    }
}
