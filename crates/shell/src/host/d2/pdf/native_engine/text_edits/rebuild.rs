//! Rebuild an edited run while retaining untouched page objects and fonts.
mod colors;
mod fold;
mod font;
mod keep;
mod stroke;
mod style;
pub(super) use colors::matched as match_char_colors;

use std::collections::{HashMap,HashSet};
use serde_json::json;
use shell_contract::{IpcValue,runtime::CancelToken};
use super::{PdfiumSession,TextObject,buffer,call,field,floats,handle,integer,matrix,number,reference,text};
use font::{Face,OwnFace};
use keep::Keep;
use style::RunStyle;

fn flag(v:&IpcValue,key:&str)->bool {matches!(field(v,key),Some(IpcValue::Bool(true))}
fn present(v:&IpcValue,key:&str)->bool {field(v,key).is_some_and(|v|!matches!(v,IpcValue::Undefined))}
fn rgb(v:Option<&IpcValue>)->Option<[u8;3]>{let IpcValue::Array(a)=v? else{return None};Some([number(a.first())? as u8,number(a.get(1))? as u8,number(a.get(2))? as u8])}
fn pair(v:Option<&IpcValue>)->Option<[f64;2]>{let IpcValue::Array(a)=v? else{return None};Some([number(a.first())?,number(a.get(1))?])}
fn offsets(v:Option<&IpcValue>)->Vec<f64>{match v{Some(IpcValue::Array(a))=>a.iter().map(|v|number(Some(v)).unwrap_or(0.0)).collect(),_=>vec![]}}
fn slice(units:&[u16],start:usize,end:usize)->Result<String,String>{String::from_utf16(&units[start..end]).map_err(|_|"invalid replacement character boundary".into())}
fn font_key(edit:&IpcValue,style:Option<&RunStyle>)->String {
    let font=style.and_then(|s|s.font.as_deref()).or_else(||text(field(edit,"newFont"))).unwrap_or("");
    let bold=style.and_then(|s|s.bold).unwrap_or_else(||flag(edit,"newBold"));
    let italic=style.and_then(|s|s.italic).unwrap_or_else(||flag(edit,"newItalic"));
    format!("{font}|{}|{}",u8::from(bold),u8::from(italic))
}
fn style_edit(edit:&IpcValue,style:&RunStyle)->IpcValue {
    let mut result=match edit {IpcValue::Object(fields)=>fields.clone(),_=>Vec::new()};
    let font=style.font.clone().or_else(||text(field(edit,"newFont")).map(str::to_owned));
    let bold=style.bold.unwrap_or_else(||flag(edit,"newBold"));
    let italic=style.italic.unwrap_or_else(||flag(edit,"newItalic"));
    result.retain(|(key,_)| !["newFont","newBold","newItalic"].contains(&key.as_str()));
    if let Some(font)=font {result.push(("newFont".into(),IpcValue::String(font)))}
    result.push(("newBold".into(),IpcValue::Bool(bold)));
    result.push(("newItalic".into(),IpcValue::Bool(italic)));
    IpcValue::Object(result)
}
fn source_size(s:&mut PdfiumSession,obj:u64)->Result<Option<f64>,String>{
    buffer(s,4,|s,b|{if integer(call(s,"FPDFTextObj_GetFontSize",&[reference(obj),reference(b)])?)?==0{return Ok(None)}; Ok(Some(floats(s,b,1)?[0]))})
}
fn set_matrix(s:&mut PdfiumSession,obj:u64,m:[f64;6])->Result<(),String>{buffer(s,24,|s,b|{let bytes=m.into_iter().flat_map(|v|(v as f32).to_le_bytes()).collect::<Vec<_>>();s.write(b,0,&bytes).map_err(|e|e.to_string())?;call(s,"FPDFPageObj_SetMatrix",&[reference(obj),reference(b)])?;Ok(())})}
fn fill(s:&mut PdfiumSession,obj:u64,c:[u8;4])->Result<(),String>{call(s,"FPDFPageObj_SetFillColor",&[reference(obj),json!(c[0]),json!(c[1]),json!(c[2]),json!(c[3])])?;Ok(())}

struct Move { obj:u64, dx:f64, dy:f64, color:Option<[u8;3]> }
struct Builder<'a> {
    session:&'a mut PdfiumSession, doc:u64, edit:&'a IpcValue, matrix:[f64;6], size:f64, color:[u8;4],
    inherited:Option<stroke::Stroke>, base:Face, own:Option<OwnFace>, faces:HashMap<String,Face>,
    objects:Vec<u64>, anchors:Vec<u64>, moves:Vec<Move>, last:u64,
}
impl Builder<'_> {
    fn face(&self,style:Option<&RunStyle>)->&Face {self.faces.get(&font_key(self.edit,style)).unwrap_or(&self.base)}
    fn size(&self,style:Option<&RunStyle>)->f64 {style.and_then(|s|s.size).filter(|n|*n>0.0).unwrap_or(self.size)}
    fn own_glyph(&self,ch:char,style:Option<&RunStyle>)->bool {self.own.as_ref().is_some_and(|o|o.covered.contains(&ch)) && font_key(self.edit,style)==font_key(self.edit,None)}
    fn advance(&mut self,ch:char,style:Option<&RunStyle>)->Result<Option<f64>,String>{
        let face=if self.own_glyph(ch,style){self.own.as_ref().map(|o|o.face.handle).unwrap_or(self.base.handle)}else{self.face(style).handle};
        let size=self.size(style);
        buffer(self.session,4,|s,b|{
            if integer(call(s,"FPDFFont_GetGlyphWidth",&[reference(face),json!(ch as u32),json!(1),reference(b)])?)?==0{return Ok(None)}
            Ok(Some(floats(s,b,1)?[0]*size))
        })
    }
    fn make_object(&mut self,value:&str,x:f64,y:f64,style:Option<&RunStyle>,own:bool)->Result<(),String>{
        let face=if own{self.own.as_ref().ok_or("missing document subset")?.face.clone()}else{self.face(style).clone()};
        let size=self.size(style);
        let obj=handle(call(self.session,"FPDFPageObj_CreateTextObj",&[reference(self.doc),reference(face.handle),json!(size)])?)?.ok_or("PDFium could not create a text object")?;
        self.objects.push(obj); self.anchors.push(self.last);
        let bytes=value.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        let ok=buffer(self.session,bytes.len(),|s,b|{s.write(b,0,&bytes).map_err(|e|e.to_string())?;integer(call(s,"FPDFText_SetText",&[reference(obj),reference(b)])?)})?;
        if ok==0{return Err("FPDFText_SetText failed on rebuilt object".into())}
        let mut m=self.matrix;m[4]=x;m[5]=y;set_matrix(self.session,obj,m)?;
        let color=style.and_then(|s|s.color).map(|c|[c[0],c[1],c[2],255]).unwrap_or(self.color);
        fill(self.session,obj,color)?;
        let width=size*self.matrix[0].hypot(self.matrix[1])*0.035;
        if !own && face.synthetic {stroke::apply(self.session,obj,color,width,2)?}
        else if let Some(st)=self.inherited {if st.same_as_fill{stroke::apply(self.session,obj,color,width,st.mode)?}else{stroke::apply(self.session,obj,st.color,st.width,st.mode)?}}
        Ok(())
    }
    fn make_segment(&mut self,value:&str,x:f64,y:f64,style:Option<&RunStyle>)->Result<(),String>{
        let mut pieces:Vec<(String,bool,f64)>=Vec::new();let mut advance=0.0;
        if self.own.is_some(){for ch in value.chars(){
            let own=self.own_glyph(ch,style);
            if let Some(last)=pieces.last_mut().filter(|p|p.1==own){last.0.push(ch)}else{pieces.push((ch.to_string(),own,advance))}
            match self.advance(ch,style)?{Some(n)=>advance+=n,None=>{pieces.clear();break}}
        }}
        if pieces.is_empty(){return self.make_object(value,x,y,style,false)}
        for (value,own,offset) in pieces {if value.trim().is_empty(){continue}self.make_object(&value,x+self.matrix[0]*offset,y+self.matrix[1]*offset,style,own)?}
        Ok(())
    }
    fn segments(&mut self,line:&str,styles:Option<&[Option<RunStyle>]>,x:f64,y:f64,preserved:bool)->Result<(),String>{
        let mut error=None;
        let segments=style::segment_line(line,styles,|ch,style|match self.advance(ch,style){Ok(v)=>v,Err(e)=>{error=Some(e);None}});
        if let Some(error)=error{return Err(error)}
        for seg in segments {if !seg.text.is_empty() && (!preserved || !seg.text.trim().is_empty()){self.make_segment(&seg.text,x+self.matrix[0]*seg.x_pt,y+if preserved {0.0}else{self.matrix[1]*seg.x_pt},seg.style.as_ref())?}}
        Ok(())
    }
    fn redraw(&mut self,new_text:&str,styles:Option<&[Option<RunStyle>]>,origin:[f64;2],leading:f64,offsets:&[f64])->Result<(),String>{
        let mut start=0;
        for (i,line) in new_text.split('\n').enumerate(){let len=line.encode_utf16().count();let line_styles=styles.map(|s|&s[start..start+len]);start+=len+1;if line.is_empty(){continue}
            let drop=i as f64*leading;
            self.segments(line,line_styles,origin[0]+offsets.get(i).copied().unwrap_or(0.0)-self.matrix[2]*drop,origin[1]-self.matrix[3]*drop,false)?;
        }
        Ok(())
    }
    fn preserve(&mut self,new_text:&str,styles:Option<&[Option<RunStyle>]>,plan:&[Keep<'_>],origin:[f64;2],leading:f64,offsets:&[f64])->Result<bool,String>{
        let units:Vec<u16>=new_text.encode_utf16().collect();let mut sorted:Vec<_>=plan.iter().collect();sorted.sort_by_key(|k|k.start);
        let mut ki=0;let mut line_start=0;
        for (line_index,line) in new_text.split('\n').enumerate(){
            let end=line_start+line.encode_utf16().count();let baseline=origin[1]-line_index as f64*leading*self.matrix[3];
            let mut cursor=origin[0]+offsets.get(line_index).copied().unwrap_or(0.0);let mut previous:Option<(&Keep<'_>,f64)>=None;let mut k=line_start;
            while k<end{
                if let Some(keep)=sorted.get(ki).filter(|p|p.start==k){
                    if let Some((prev,placed))=previous {let between=slice(&units,prev.end,keep.start)?;if (keep.y-prev.y).abs()<0.5 && keep.x>prev.x && between.chars().all(jscompat::is_js_space){cursor=placed+keep.x-prev.x}}
                    self.moves.push(Move{obj:keep.src.handle,dx:cursor-keep.x,dy:baseline-keep.y,color:keep.color});self.last=keep.src.handle;previous=Some((keep,cursor));cursor+=keep.advance;k=keep.end;ki+=1;continue
                }
                let run_end=sorted.get(ki).map(|p|p.start.min(end)).unwrap_or(end);if run_end<=k{return Ok(false)}
                let run=slice(&units,k,run_end)?;
                if !run.trim().is_empty(){self.segments(&run,styles.map(|s|&s[k..run_end]),cursor,baseline,true)?;previous=None}
                let mut cu=k;for ch in run.chars(){let st=styles.and_then(|s|s.get(cu)).and_then(Option::as_ref);let width=match self.advance(ch,st)?{Some(width)=>width, None if jscompat::is_js_space(ch)=>self.size(st)*0.28,None=>return Ok(false)};cursor+=width*self.matrix[0];cu+=ch.len_utf16()}
                k=run_end;
            }
            line_start=end+1;
        }
        Ok(ki==sorted.len())
    }
    fn discard(&mut self){for obj in self.objects.drain(..){let _=call(self.session,"FPDFPageObj_Destroy",&[reference(obj)]);}self.anchors.clear();self.moves.clear();self.last=0;}
}

pub(super) fn rebuild(session:&mut PdfiumSession,doc:u64,page:u64,edit:&IpcValue,matches:&[TextObject],new_text:&str,text_page:u64,cancel:&CancelToken)->Result<bool,String>{
    if cancel.is_cancelled(){return Err("cancelled".into())}
    let anchor=matches.iter().min_by(|a,b|a.bounds[0].total_cmp(&b.bounds[0])).ok_or("no matched text object")?;
    let mut mat=matrix(session,anchor.handle)?;
    let mut size=source_size(session,anchor.handle)?.unwrap_or_else(||number(field(edit,"fontSize")).unwrap_or(1.0));
    if let Some(n)=number(field(edit,"newFontSize")).filter(|n|*n>0.0){size=n}
    if let Some(origin)=pair(field(edit,"origin")){mat=[1.0,0.0,0.0,1.0,origin[0],origin[1]];size=number(field(edit,"newFontSize")).filter(|n|*n>0.0).unwrap_or_else(||number(field(edit,"fontSize")).unwrap_or(size));}
    let new_color=rgb(field(edit,"newColor"));
    let mut color=new_color.map(|c|[c[0],c[1],c[2],255]).or(stroke::color(session,anchor.handle,"FPDFPageObj_GetFillColor")?).unwrap_or([0,0,0,255]);if color[3]==0{color[3]=255}
    let inherited_colors=if new_color.is_some(){None}else{colors::matched(session,matches,new_text)?};
    let planned=style::planned_styles(edit,new_text);
    let styles=style::overlay_styles(new_text,inherited_colors.as_deref(),planned.as_deref());
    let override_face=present(edit,"newFontSize")||present(edit,"newFont")||flag(edit,"newBold")||flag(edit,"newItalic");
    let aligned=mat[1].abs()<1e-4 && mat[2].abs()<1e-4 && mat[0]>0.0 && mat[3]>0.0;
    let keeps=if !override_face && aligned {keep::plan(session,text_page,matches,new_text,styles.as_deref(),new_color,edit)?}else{None};
    let units:Vec<u16>=new_text.encode_utf16().collect();
    let redraw_text=if let Some(plan)=keeps.as_ref(){let mut sorted:Vec<_>=plan.iter().collect();sorted.sort_by_key(|k|k.start);let mut parts=String::new();let mut from=0;for keep in sorted{parts.push_str(&slice(&units,from,keep.start)?);from=keep.end}parts.push_str(&slice(&units,from,units.len())?);parts}else{new_text.to_owned()};
    let mut cff=false;
    let base=font::load(session,doc,anchor.font,edit,&redraw_text,cancel,&mut cff)?;
    let own=if override_face{None}else{font::partial(session,doc,anchor.font,&redraw_text,&mut cff)?};
    let inherited=stroke::read(session,anchor.handle)?;
    let base_key=font_key(edit,None);let mut grouped:Vec<(String,RunStyle,String)>=Vec::new();
    if let Some(styles)=styles.as_ref(){let mut pos=0;for ch in new_text.chars(){let st=styles.get(pos).and_then(Option::as_ref);if let Some(st)=st{let key=font_key(edit,Some(st));if key!=base_key{if let Some((_,_,text))=grouped.iter_mut().find(|g|g.0==key){text.push(ch)}else{grouped.push((key,st.clone(),ch.to_string()))}}}pos+=ch.len_utf16();}}
    let mut faces=HashMap::new();for (key,style,drawn) in grouped {let edit=style_edit(edit,&style);faces.insert(key,font::load(session,doc,anchor.font,&edit,&drawn,cancel,&mut cff)?);}
    let origin=pair(field(edit,"origin")).unwrap_or([mat[4],mat[5]]);let leading=number(field(edit,"lineLeading")).unwrap_or(size*1.2);let offsets=offsets(field(edit,"lineXOffsets"));
    let mut b=Builder{session,doc,edit,matrix:mat,size,color,inherited,base,own,faces,objects:Vec::new(),anchors:Vec::new(),moves:Vec::new(),last:0};
    let build=(||->Result<(),String>{
        if let Some(plan)=keeps.as_ref(){if !b.preserve(new_text,styles.as_deref(),plan,origin,leading,&offsets)?{b.discard();b.base=font::load(b.session,doc,anchor.font,edit,new_text,cancel,&mut cff)?;b.own=if override_face{None}else{font::partial(b.session,doc,anchor.font,new_text,&mut cff)?};b.redraw(new_text,styles.as_deref(),origin,leading,&offsets)?}}
        else{b.redraw(new_text,styles.as_deref(),origin,leading,&offsets)?}
        Ok(())
    })();
    if let Err(error)=build{b.discard();return Err(error)}
    let indexed=match b.session.supports("FPDFPage_InsertObjectAtIndex"){Ok(value)=>value,Err(error)=>{b.discard();return Err(error.to_string())}};
    let kept:HashSet<_>=b.moves.iter().map(|m|m.obj).collect();
    for mv in &b.moves {let mut m=matrix(b.session,mv.obj)?;m[4]+=mv.dx;m[5]+=mv.dy;set_matrix(b.session,mv.obj,m)?;if let Some(c)=mv.color{let st=stroke::read(b.session,mv.obj)?;let color=[c[0],c[1],c[2],255];fill(b.session,mv.obj,color)?;if st.is_some_and(|s|s.same_as_fill){call(b.session,"FPDFPageObj_SetStrokeColor",&[reference(mv.obj),json!(c[0]),json!(c[1]),json!(c[2]),json!(255)])?;}}}
    let removed:Vec<_>=matches.iter().filter(|o|!kept.contains(&o.handle)).collect();
    for obj in &removed{call(b.session,"FPDFPage_RemoveObject",&[reference(page),reference(obj.handle)])?;call(b.session,"FPDFPageObj_Destroy",&[reference(obj.handle)])?;}
    if indexed&&!kept.is_empty()&&!b.objects.is_empty(){
        let count=integer(call(b.session,"FPDFPage_CountObjects",&[reference(page)])?)?;let mut indices=HashMap::new();for i in 0..count{if let Some(id)=handle(call(b.session,"FPDFPage_GetObject",&[reference(page),json!(i)])?)?{indices.insert(id,i);}}
        let start=kept.iter().filter_map(|id|indices.get(id)).copied().min().unwrap_or(0);let mut groups:Vec<(i64,Vec<u64>)>=Vec::new();
        for (&obj,&anchor) in b.objects.iter().zip(&b.anchors){let at=indices.get(&anchor).map(|i|i+1).unwrap_or(start);if let Some(last)=groups.last_mut().filter(|g|g.0==at){last.1.push(obj)}else{groups.push((at,vec![obj]))}}
        groups.sort_by(|a,b|b.0.cmp(&a.0));for (at,objects) in groups{for (i,obj) in objects.iter().enumerate(){call(b.session,"FPDFPage_InsertObjectAtIndex",&[reference(page),reference(*obj),json!(at+i as i64)])?;}}
    }else{
        let at=removed.iter().map(|o|o.index).min();for (i,obj) in b.objects.iter().enumerate(){if let Some(at)=at.filter(|_|indexed){call(b.session,"FPDFPage_InsertObjectAtIndex",&[reference(page),reference(*obj),json!(at+i)])?;}else{call(b.session,"FPDFPage_InsertObject",&[reference(page),reference(*obj)])?;}}
    }
    Ok(cff)
}
