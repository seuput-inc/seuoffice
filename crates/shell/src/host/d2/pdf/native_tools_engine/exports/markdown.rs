//! Reflow a structured PDF tree into Markdown and owned image assets.
mod inline;
use std::collections::HashMap;
use sha2::{Digest, Sha256};
use shell_contract::IpcValue;
use shell_ipc_d2::pdf::tools::JobFailure;

pub(super) struct Asset { pub name:String, pub sha256:String, pub data:Vec<u8> }
#[derive(Clone)]
pub(super) struct AssetRecord { pub name:String, pub sha256:String }
pub(super) struct MarkdownConversion { pub markdown:String, pub warnings:IpcValue, pub assets:Vec<Asset>, pub manifest_entries:Vec<AssetRecord> }
fn field<'a>(v:&'a IpcValue,key:&str)->Option<&'a IpcValue>{match v{IpcValue::Object(fields)=>fields.iter().find(|(k,_)|k==key).map(|(_,v)|v),_=>None}}
fn string<'a>(v:&'a IpcValue,key:&str)->&'a str {match field(v,key){Some(IpcValue::String(s))=>s,_=>""}}
fn number(v:&IpcValue,key:&str)->f64 {match field(v,key){Some(IpcValue::Number(n))=>*n,_=>0.}}
fn array<'a>(v:&'a IpcValue,key:&str)->&'a [IpcValue]{match field(v,key){Some(IpcValue::Array(a))=>a,_=>&[]}}
fn flag(v:&IpcValue,key:&str)->bool{matches!(field(v,key),Some(IpcValue::Bool(true)))}
fn failure(s:&str)->JobFailure{JobFailure::new("failed",Some(s))}
fn warning(kind:&str,page:Option<f64>,id:Option<&str>,message:String)->IpcValue{let mut f=vec![("kind".into(),IpcValue::String(kind.into()))];if let Some(page)=page{f.push(("page".into(),IpcValue::Number(page)));}if let Some(id)=id{f.push(("footnoteId".into(),IpcValue::String(id.into())));}f.push(("message".into(),IpcValue::String(message)));IpcValue::Object(f)}
fn lines(s:&str)->String{s.split('\n').enumerate().map(|(i,line)|inline::escape_line_start(line,i)).collect::<Vec<_>>().join("\n")}
fn indent(s:&str,n:usize)->String{s.replace('\n',&format!("\n{}"," ".repeat(n)))}
#[derive(Default)] struct Lists{cols:Vec<usize>,counters:Vec<usize>,ordered:Vec<bool>}
struct Writer<'a>{tree:&'a IpcValue, parts:Vec<String>, assets:Vec<Asset>, asset_index:HashMap<String,usize>, warnings:Vec<IpcValue>,notes:HashMap<String,&'a IpcValue>, note_order:Vec<String>, numbers:HashMap<String,usize>, list:Option<Lists>, page:f64}
impl inline::Host for Writer<'_>{
 fn footnote_number(&mut self,id:&str)->Option<usize>{if !self.notes.contains_key(id){return None}if let Some(n)=self.numbers.get(id){return Some(*n)}let n=self.note_order.len()+1;self.note_order.push(id.into());self.numbers.insert(id.into(),n);Some(n)}
 fn missing_footnote(&mut self,id:&str){self.warnings.push(warning("footnoteMissing",Some(self.page),Some(id),format!("a footnote marker on page {} has no note",self.page)));}
 fn formula_as_code(&mut self,_:&str){self.warnings.push(warning("formulaAsCode",Some(self.page),None,format!("a formula on page {} was written as code",self.page)));}
}
impl<'a> Writer<'a>{
 fn push(&mut self,s:String){if !s.is_empty(){self.list=None;self.parts.push(s);}}
 fn inline(&mut self,runs:&[IpcValue],mode:inline::Mode,italic:bool)->Result<String,JobFailure>{inline::render(runs,mode,self,italic)}
 fn picture(&mut self,b:&IpcValue)->Result<String,JobFailure>{let data=match field(b,"data"){Some(IpcValue::Bytes(_,d))=>d,_=>return Err(failure("image data is missing"))};let mime=string(b,"mime");let ext=match mime{"image/png"=>"png","image/jpeg"=>"jpg",_=>return Err(failure("unsupported image type"))};let hash=format!("{:x}",Sha256::digest(data));let index=if let Some(i)=self.asset_index.get(&hash){*i}else{let i=self.assets.len();self.assets.push(Asset{name:format!("{hash}.{ext}"),sha256:hash.clone(),data:data.clone()});self.asset_index.insert(hash,i);i};Ok(format!("![](assets/{})",self.assets[index].name))}
 fn caption(&mut self,b:Option<&IpcValue>)->Result<(),JobFailure>{if let Some(b)=b{let value=self.inline(array(b,"runs"),inline::Mode::Block,true)?;self.push(lines(&value));}Ok(())}
 fn list_item(&mut self,b:&IpcValue)->Result<(),JobFailure>{let text=self.inline(array(b,"runs"),inline::Mode::Block,false)?;if text.is_empty(){return Ok(())}let ordered=flag(b,"ordered");let level=number(b,"level").floor().max(0.) as usize;let mut list=self.list.take().unwrap_or_default();let continuing=!list.cols.is_empty()&&!(level==0&&list.ordered[0]!=ordered);if !continuing{list=Lists::default();}let level=level.min(list.cols.len());let continued=level<list.cols.len()&&list.ordered[level]==ordered;let marker=string(b,"marker").trim();let digits=marker.trim_start_matches('(').chars().take_while(char::is_ascii_digit).collect::<String>();let start=if digits.len()<=9{digits.parse::<usize>().unwrap_or(1)}else{1};let n=if continued{list.counters[level]+1}else{start};let indent_at=if level==0{0}else{list.cols[level-1]};let marker=if ordered{format!("{n}.")}else{"-".into()};list.cols.truncate(level);list.counters.truncate(level);list.ordered.truncate(level);list.cols.push(indent_at+marker.len()+1);list.counters.push(n);list.ordered.push(ordered);let line=format!("{}{marker} {}"," ".repeat(indent_at),indent(&lines(&text),indent_at+marker.len()+1));if continuing{if let Some(last)=self.parts.last_mut(){last.push('\n');last.push_str(&line)}else{self.parts.push(line)}}else{self.parts.push(line)}self.list=Some(list);Ok(())}
 fn table(&mut self,b:&IpcValue)->Result<(),JobFailure>{
  let table=field(b,"table").ok_or_else(||failure("table is missing"))?;
  let rows=number(table,"rowCount").max(0.) as usize;let cols=number(table,"colCount").max(0.) as usize;
  if rows>0&&cols>0{
   let mut grid=vec![vec![String::new();cols];rows];
   for cell in array(table,"cells"){
    let row=number(cell,"row");let col=number(cell,"col");
    if row<0.||col<0.||row>=rows as f64||col>=cols as f64{continue}
    let mut runs=Vec::new();for (i,p) in array(cell,"paragraphs").iter().enumerate(){
     if i>0{runs.push(IpcValue::Object(vec![("kind".into(),IpcValue::String("lineBreak".into()))]));}
     if let IpcValue::Array(a)=p{runs.extend(a.iter().cloned());}
    }
    grid[row as usize][col as usize]=self.inline(&runs,inline::Mode::Table,false)?;
   }
   let row=|v:&[String]|format!("| {} |",v.join(" | "));
   let mut result=vec![row(&grid[0]),row(&vec!["---".to_owned();cols])];
   result.extend(grid.iter().skip(1).map(|r|row(r)));self.push(result.join("\n"));
  }
  self.caption(field(b,"caption"))
 }
 fn run(mut self)->Result<MarkdownConversion,JobFailure>{for b in array(self.tree,"blocks"){self.page=number(b,"page");match string(b,"kind"){
 "pageBreak"=>{},"heading"=>{let runs=array(b,"runs");let texts=runs.iter().filter(|r|string(r,"kind")=="text"&&!string(r,"text").trim().is_empty()).collect::<Vec<_>>();let all_bold=!texts.is_empty()&&texts.iter().all(|r|flag(r,"bold"));let mut adjusted=runs.to_vec();if all_bold{for r in &mut adjusted{if let IpcValue::Object(fields)=r{fields.retain(|(k,_)|k!="bold");}}}let text=self.inline(&adjusted,inline::Mode::Heading,false)?;if !text.is_empty(){let text=if let Some((prefix,hashes))=text.rsplit_once(' '){if !hashes.is_empty()&&hashes.chars().all(|c|c=='#'){format!("{prefix} \\{hashes}")}else{text}}else{text};self.push(format!("{} {text}","#".repeat((number(b,"level") as usize).min(6))));}},
 "paragraph"=>{if field(b,"figure").is_none(){let s=self.inline(array(b,"runs"),inline::Mode::Block,false)?;self.push(lines(&s));}},"listItem"=>self.list_item(b)?,"table"=>self.table(b)?,
 "image"=>{let empty=matches!(field(b,"data"),Some(IpcValue::Bytes(_,d)) if d.is_empty());if flag(b,"placeholder")||empty{self.warnings.push(warning("figureWithoutImage",Some(self.page),None,format!("a figure on page {} has no image data",self.page)));}else{let image=self.picture(b)?;self.push(image)}self.caption(field(b,"caption"))?;self.caption(field(b,"captionTranslation"))?;},
 "pageImage"=>{let image=self.picture(b)?;self.push(image);let reason=string(b,"reason");let mut w=match warning("pageAsImage",Some(self.page),None,format!("page {} was inserted as an image",self.page)){IpcValue::Object(f)=>f,_=>unreachable!()};w.insert(2,("reason".into(),IpcValue::String(reason.into())));self.warnings.push(IpcValue::Object(w));},
 "math"=>{let latex=string(b,"latex").lines().map(str::trim_end).filter(|l|!l.trim().is_empty()).collect::<Vec<_>>().join("\n").trim().to_owned();if !latex.is_empty(){if latex.contains('$'){self.formula_as_code(&latex);self.push(inline::fence_code_block(&latex,"latex"));}else{self.push(format!("$$\n{latex}\n$$"));}}},_=>return Err(failure("unknown PDF structure block")),}}
 let mut next=0;loop{while next<self.note_order.len(){let id=self.note_order[next].clone();next+=1;let note=self.notes[&id];self.page=number(note,"page");let mut paragraphs=Vec::new();for p in array(note,"blocks"){let text=self.inline(array(p,"runs"),inline::Mode::Block,false)?;if !text.is_empty(){paragraphs.push(lines(&text));}}let body=paragraphs.iter().enumerate().map(|(i,s)|format!("{}{}",if i==0{""}else{"    "},indent(s,4))).collect::<Vec<_>>().join("\n\n");self.push(format!("[^{}]:{}{}",self.numbers[&id],if body.is_empty(){""}else{" "},body));}let missing=array(self.tree,"footnotes").iter().find(|n|!self.numbers.contains_key(string(n,"id")));if let Some(note)=missing{let id=string(note,"id");self.warnings.push(warning("footnoteUnreferenced",Some(number(note,"page")),Some(id),format!("a footnote on page {} has no marker in the text",number(note,"page"))));self.footnote_number(id);}else{break;}}
 for p in array(self.tree,"footnoteLinkFailedPages"){if let IpcValue::Number(page)=p{self.warnings.push(warning("footnoteLinkFailed",Some(*page),None,format!("footnotes on page {page} could not be linked to their markers")));}}
 let pages=array(self.tree,"pages").iter().filter(|p|string(p,"status")!="toc").collect::<Vec<_>>();if !pages.is_empty()&&pages.iter().all(|p|matches!(string(p,"status"),"scanned"|"degraded")){self.warnings.push(warning("allPagesAsImages",None,None,"every page was inserted as an image; reading the pages with AI can recover the text".into()));}
 let markdown=if self.parts.is_empty(){String::new()}else{format!("{}\n",self.parts.join("\n\n"))};let manifest_entries=self.assets.iter().map(|a|AssetRecord{name:a.name.clone(),sha256:a.sha256.clone()}).collect();Ok(MarkdownConversion{markdown,warnings:IpcValue::Array(self.warnings),assets:self.assets,manifest_entries})}
}
use inline::Host as _;
pub(super) fn convert(tree:&IpcValue)->Result<MarkdownConversion,JobFailure>{let mut notes=HashMap::new();for n in array(tree,"footnotes"){notes.entry(string(n,"id").to_owned()).or_insert(n);}Writer{tree,parts:Vec::new(),assets:Vec::new(),asset_index:HashMap::new(),warnings:Vec::new(),notes,note_order:Vec::new(),numbers:HashMap::new(),list:None,page:1.}.run()}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::BytesKind;
    fn obj(fields:Vec<(&str,IpcValue)>)->IpcValue{IpcValue::Object(fields.into_iter().map(|(k,v)|(k.into(),v)).collect())}
    #[test]
    fn pictures_are_deduplicated_and_warning_retains_page() {
        let img=obj(vec![("kind",IpcValue::String("pageImage".into())),("page",IpcValue::Number(2.)),("reason",IpcValue::String("scanned".into())),("mime",IpcValue::String("image/png".into())),("data",IpcValue::Bytes(BytesKind::Uint8Array,vec![1,2,3]))]);
        let tree=obj(vec![("blocks",IpcValue::Array(vec![img.clone(),img])),("pages",IpcValue::Array(vec![])),("footnotes",IpcValue::Array(vec![]))]);
        let converted=convert(&tree).unwrap();
        assert_eq!(converted.assets.len(),1);
        assert_eq!(converted.markdown.matches("![](assets/").count(),2);
        assert!(matches!(&converted.warnings,IpcValue::Array(a) if a.len()==2));
    }
}
