//! Recognize nested bullets and ordered runs without converting headings to lists.
use std::collections::HashMap;
use super::super::geometry::{median, Rect};
use super::super::ir::{Alignment, Direction, Line, ListInfo, TextBlock};
use super::super::{rtl,script};

#[derive(Clone,Debug)]
struct Marker {kind:&'static str,weak:bool,value:Option<i64>,values:Option<Vec<i64>>,style:Option<&'static str>,text:String}
#[derive(Clone)]
struct Item {src:usize,lines:Vec<Line>,marker:Marker,x:f64,font:f64,hanging:bool}
struct Exploded {block:TextBlock,prefix:Vec<Line>,items:Vec<usize>}
#[derive(Clone,Debug,PartialEq)]
pub(super) struct LastOrdered {pub value:i64,pub style:String,pub level:usize,pub sequence_id:usize,pub start:i64}
#[derive(Default)]
pub(super) struct ListSeq {pub next:usize,pub last_ordered:Option<LastOrdered>}
fn line_text(line:&Line)->String{line.spans.iter().map(|s|s.text.as_str()).collect()}
fn parse(line:&Line)->Option<Marker>{
    let text=line_text(line);let first=text.chars().next()?;let mut end=first.len_utf8();let mut values=None;let (kind,weak,style,value);
    if "•◦▪▫‣∙·●○■□◆◇".contains(first){kind="bullet";weak=false;style=None;value=None;}
    else if "–—−-".contains(first){kind="bullet";weak=true;style=None;value=None;}
    else {
        let bytes=text.as_bytes();let mut p=0;let parens=first=='(';if parens{p+=1;}
        let mut numbers=Vec::new();loop{let start=p;while p<bytes.len()&&bytes[p].is_ascii_digit()&&p-start<3{p+=1;}if p==start{return None;}numbers.push(text[start..p].parse::<i64>().ok()?);if p>=bytes.len(){return None;}if bytes[p]==b'.'&&p+1<bytes.len()&&bytes[p+1].is_ascii_digit()&&!parens&&numbers.len()<3{p+=1;continue;}break;}
        if parens {if bytes[p]!=b')'{return None;}p+=1;style=Some("parens");}
        else if numbers.len()>1 {if bytes[p]!=b'.'{return None;}p+=1;style=Some("multi");}
        else {style=match bytes[p]{b'.'=>Some("dot"),b')'=>Some("paren"),_=>return None};p+=1;}
        value=numbers.last().copied();if numbers.len()>1{values=Some(numbers);}end=p;kind="ordered";weak=false;
    }
    let before_space=end;for c in text[end..].chars(){if !c.is_whitespace(){break;}end+=c.len_utf8();}
    if weak {if end==before_space||end==text.len(){return None;}}
    else if end==before_space {let next=text[end..].chars().next()?;if !script::east_asian(script::script_of(next as u32)){return None;}}
    Some(Marker{kind,weak,value,values,style,text:text[..end].into()})
}
fn strip_marker(line:&Line,mut remaining:usize)->Line{let mut out=line.clone();out.spans.clear();let mut boundary=true;for span in &line.spans{let mut s=span.clone();let take=remaining.min(s.text.len());let take=(0..=take).rev().find(|&i|s.text.is_char_boundary(i)).unwrap_or(0);s.text=s.text[take..].into();remaining-=take;if remaining==0&&boundary&&!s.text.is_empty(){s.text=s.text.trim_start().into();if !s.text.is_empty(){boundary=false;}}if !s.text.is_empty(){out.spans.push(s);}}out}
fn block(lines:Vec<Line>,src:&TextBlock,prefix:bool,info:Option<ListInfo>)->TextBlock{let bounds=lines.iter().map(|l|l.bounds).reduce(Rect::union).unwrap_or_default();let direction=rtl::first_strong_dir(&lines);TextBlock{lines,bounds,align:if info.is_some(){Alignment::Left}else{src.align},first_line_indent_pt:if prefix{src.first_line_indent_pt}else{0.0},direction,list:info,stitched_from_line:None,toc_entry:None,spacing_before_pt:None,border:None,card_id:None}}
fn info(item:&Item,level:usize,seq:Option<usize>,start:Option<i64>)->ListInfo{ListInfo{kind:item.marker.kind.into(),level,sequence_id:seq,start,style:item.marker.style.map(str::to_owned),start_values:item.marker.values.clone(),marker:item.marker.text.clone()}}
fn level(x:f64,levels:&[f64],tol:f64)->usize{levels.iter().position(|&l|(x-l).abs()<=tol).unwrap_or(levels.len())}
fn validate(ids:&[usize],items:&[Item],seq:&mut ListSeq,body:Option<f64>,is_rtl:bool,accepted:&mut HashMap<usize,ListInfo>){
    let em=median(&ids.iter().map(|&i|items[i].font).collect::<Vec<_>>());let tol=0.6*if em==0.0{12.0}else{em};let mut levels=Vec::new();for &i in ids{if level(items[i].x,&levels,tol)==levels.len(){levels.push(items[i].x);}}levels.sort_by(|a,b|if is_rtl{b.total_cmp(a)}else{a.total_cmp(b)});
    let mut multi:Vec<usize>=Vec::new();let flush_multi=|run:&mut Vec<usize>,seq:&mut ListSeq,accepted:&mut HashMap<usize,ListInfo>| {if run.len()>=2{let id=seq.next;seq.next+=1;let first=items[run[0]].marker.values.as_ref().unwrap();for &i in run.iter(){let mut v=info(&items[i],first.len()-1,Some(id),first.last().copied());v.start_values=Some(first.clone());accepted.insert(i,v);}}run.clear();};
    for &i in ids.iter().filter(|&&i|items[i].marker.style==Some("multi")){let v=items[i].marker.values.as_ref().unwrap();let follows=multi.last().is_some_and(|&p|{let prev=items[p].marker.values.as_ref().unwrap();v.len()==prev.len()&&v[..v.len()-1]==prev[..prev.len()-1]&&v[v.len()-1]==prev[prev.len()-1]+1});if !follows{flush_multi(&mut multi,seq,accepted);}multi.push(i);}flush_multi(&mut multi,seq,accepted);
    let mut ending:Option<(f64,LastOrdered)>=None;
    for lv in 0..levels.len().min(3){let at:Vec<_>=ids.iter().copied().filter(|&i|level(items[i].x,&levels,tol)==lv&&items[i].marker.style!=Some("multi")).collect();let bullets:Vec<_>=at.iter().copied().filter(|&i|items[i].marker.kind=="bullet").collect();let strong=bullets.iter().filter(|&&i|!items[i].marker.weak).count();let hanging=bullets.iter().any(|&i|items[i].hanging);let indented=|i:usize|!is_rtl&&body.is_some_and(|b|items[i].x-b>=1.2*items[i].font.max(1.0));let weak=bullets.iter().filter(|&&i|items[i].marker.weak&&indented(i)).count();for i in bullets{let item=&items[i];if if item.marker.weak{hanging||weak>=2&&indented(i)}else{strong>=2||item.hanging}{accepted.insert(i,info(item,lv,None,None));}}
        let ordered:Vec<_>=at.into_iter().filter(|&i|items[i].marker.kind=="ordered"&&(items[i].lines.len()<2||items[i].hanging)).collect();let mut run:Vec<usize>=Vec::new();
        let flush=|run:&mut Vec<usize>,seq:&mut ListSeq,accepted:&mut HashMap<usize,ListInfo>,ending:&mut Option<(f64,LastOrdered)>|{if run.is_empty(){return;}let first=&items[run[0]];let last=&items[*run.last().unwrap()];let previous=seq.last_ordered.as_ref().filter(|p|p.level==lv&&Some(p.style.as_str())==first.marker.style&&Some(p.value+1)==first.marker.value).cloned();let id_start=if run.len()>=2{let id=seq.next;seq.next+=1;Some((id,first.marker.value.unwrap()))}else{previous.map(|p|(p.sequence_id,p.start))};if let Some((id,start))=id_start{for &i in run.iter(){accepted.insert(i,info(&items[i],lv,Some(id),Some(start)));}let end=LastOrdered{value:last.marker.value.unwrap(),style:last.marker.style.unwrap().into(),level:lv,sequence_id:id,start};let y=last.lines.last().unwrap().bounds.y0;if ending.as_ref().is_none_or(|(old,_)|y<*old){*ending=Some((y,end));}}run.clear();};
        for i in ordered{if !run.last().is_some_and(|&p|items[i].marker.value==items[p].marker.value.map(|v|v+1)&&items[i].marker.style==items[p].marker.style){flush(&mut run,seq,accepted,&mut ending);}run.push(i);}flush(&mut run,seq,accepted,&mut ending);
    }
    if let Some((_,last))=ending{seq.last_ordered=Some(last);}
}
pub(super) fn detect_list_blocks(blocks:Vec<TextBlock>,seq:&mut ListSeq,fallback_body:Option<f64>)->Vec<TextBlock>{
    let mut items:Vec<Item>=Vec::new();let mut exploded=Vec::new();
    for (src,b) in blocks.iter().enumerate(){let mut prefix=Vec::new();let mut indexes=Vec::new();for line in &b.lines{if let Some(marker)=parse(line){let sizes:Vec<_>=line.spans.iter().map(|s|s.font_size).collect();let fs=median(&sizes);let fs=if fs==0.0{12.0}else{fs};let x=if b.direction==Direction::Rtl{line.bounds.x1}else{line.bounds.x0};let i=items.len();items.push(Item{src,lines:vec![line.clone()],marker,x,font:fs,hanging:false});indexes.push(i);}else if let Some(&i)=indexes.last(){let item:&mut Item=&mut items[i];let indent=if b.direction==Direction::Rtl{(item.x-line.bounds.x1)/item.font}else{(line.bounds.x0-item.x)/item.font};if (0.8..=6.0).contains(&indent){item.hanging=true;}item.lines.push(line.clone());}else{prefix.push(line.clone());}}exploded.push(Exploded{block:b.clone(),prefix,items:indexes});}
    if items.is_empty(){return blocks;}
    let plain:Vec<_>=exploded.iter().filter(|e|e.items.is_empty()&&!e.block.lines.is_empty()).map(|e|if e.block.direction==Direction::Rtl{e.block.bounds.x1}else{e.block.bounds.x0}).collect();let body=if plain.is_empty(){fallback_body}else{Some(median(&plain))};
    let mut regions:Vec<Vec<usize>>=Vec::new();let mut region=Vec::new();for e in &exploded{if (e.items.is_empty()||!e.prefix.is_empty())&&!region.is_empty(){regions.push(std::mem::take(&mut region));}region.extend(&e.items);}if !region.is_empty(){regions.push(region);}
    let mut accepted=HashMap::new();for r in regions{validate(&r,&items,seq,body,blocks[items[r[0]].src].direction==Direction::Rtl,&mut accepted);}if accepted.is_empty(){return blocks;}
    let mut out=Vec::new();for e in exploded{if !e.items.iter().any(|i|accepted.contains_key(i)){out.push(e.block);continue;}if !e.prefix.is_empty(){out.push(block(e.prefix,&e.block,true,None));}let mut plain=Vec::new();for i in e.items{let item=&items[i];if let Some(info)=accepted.remove(&i){if !plain.is_empty(){out.push(block(std::mem::take(&mut plain),&e.block,false,None));}let mut lines=item.lines.clone();lines[0]=strip_marker(&lines[0],item.marker.text.len());lines.retain(|l|!l.spans.is_empty());if lines.is_empty(){lines=item.lines.clone();}let mut b=block(lines,&e.block,false,Some(info));b.bounds=item.lines.iter().map(|l|l.bounds).reduce(Rect::union).unwrap_or_default();out.push(b);}else{plain.extend(item.lines.clone());}}if !plain.is_empty(){out.push(block(plain,&e.block,false,None));}}out
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::ir::Span;
    use super::super::super::script::Script;
    fn item(text:&str,y:f64)->TextBlock {
        let bounds=Rect{x0:10.0,y0:y,x1:100.0,y1:y+10.0};
        let span=Span{text:text.into(),bounds,font_size:10.0,font_family:"Arial".into(),bold:false,italic:false,color:"000000".into(),direction:Direction::Ltr,script:Script::Latin,highlight:None,underline:false,strike:false,char_scale:None,char_spacing_pt:None,note_ref:None,invisible:false,href:None};
        let line=Line{spans:vec![span],bounds,baseline:y,ends_with_hyphen:false,hard_break_before:false};
        TextBlock{lines:vec![line],bounds,align:Alignment::Left,first_line_indent_pt:0.0,direction:Direction::Ltr,list:None,stitched_from_line:None,toc_entry:None,spacing_before_pt:None,border:None,card_id:None}
    }
    #[test]
    fn consecutive_ordered_items_are_real_lists_but_single_heading_is_not(){
        let mut seq=ListSeq::default();
        let result=detect_list_blocks(vec![item("1. Alpha",80.0),item("2. Beta",60.0)],&mut seq,None);
        assert_eq!(result.len(),2);
        assert_eq!(result[0].list.as_ref().unwrap().start,Some(1));
        assert_eq!(result[1].lines[0].spans[0].text,"Beta");
        assert_eq!(result[0].list.as_ref().unwrap().sequence_id,result[1].list.as_ref().unwrap().sequence_id);
        let heading=detect_list_blocks(vec![item("4. Heading",40.0)],&mut ListSeq::default(),None);
        assert!(heading[0].list.is_none());
    }
}
