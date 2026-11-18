//! Markdown inline and block syntax matching the editor's tokenizer.
use jscompat::TryJsRegex;
use shell_contract::IpcValue;
use shell_ipc_d2::pdf::tools::JobFailure;
use super::{flag,string};

pub(super) trait Host { fn footnote_number(&mut self,id:&str)->Option<usize>; fn missing_footnote(&mut self,id:&str); fn formula_as_code(&mut self,latex:&str); }
#[derive(Clone,Copy,PartialEq,Eq)] pub(super) enum Mode{Block,Heading,Table}
#[derive(Clone,Default,PartialEq,Eq)] struct Marks{bold:bool,italic:bool,href:Option<String>}
#[derive(Clone,PartialEq,Eq)] enum Kind{Text,Math,Footnote,Marker,Break}
#[derive(Clone)] struct Seg{kind:Kind,text:String,marks:Marks}
fn regex(p:&str)->TryJsRegex{TryJsRegex::new(p,if p.contains("\\p{"){"u"}else{""}).expect("static Markdown regex")}
fn test(p:&str,s:&str)->bool{regex(p).try_test(s).expect("Markdown regex execution")}
fn replace<F:FnMut(&str,&[Option<String>])->String>(s:&str,p:&str,mut f:F)->String{let r=regex(p);let mut out=String::new();let mut at=0;for m in r.try_find_iter(s){let m=m.expect("Markdown regex execution");out.push_str(&s[at..m.start()]);let captures=(1..5).map(|n|m.get(n).map(str::to_owned)).collect::<Vec<_>>();out.push_str(&f(m.as_str(),&captures));at=m.end();}out.push_str(&s[at..]);out}
fn intersection(a:Option<&Marks>,b:Option<&Marks>)->Marks{match(a,b){(Some(a),Some(b))=>Marks{bold:a.bold&&b.bold,italic:a.italic&&b.italic,href:if a.href==b.href{a.href.clone()}else{None}},_=>Marks::default()}}
fn risky(s:&str)->bool{test(r"\]\(|^\s*\[[ xX]\]|^\s*\[[^\]]+\]:|\[\^",s)}
fn math_syntax(s:&str)->bool{test(r"(?<!\\)(?:\\\\)*\$\$|(?<!\\)(?:\\\\)*\$(?!\s)([^$\n]*[^\\\s$])\$(?![$\d])",s)}
fn escape(s:&str,brackets:bool,dollars:bool,table:bool)->String{let mut out=String::new();let source=replace(s,r"&(?=#?[A-Za-z0-9]+;)",|_,_|"&amp;".into());for ch in source.chars(){if matches!(ch,'\\'|'`'|'*'|'_'|'~')||(brackets&&matches!(ch,'['|']'))||(dollars&&ch=='$')||(table&&ch=='|'){out.push('\\');}match ch{'<'=>out.push_str("&lt;"),'\u{a0}'=>out.push_str("&nbsp;"),_=>out.push(ch)}}out}
pub(super) fn escape_line_start(line:&str,index:usize)->String{if index>0&&line.contains('|')&&test(r"^\s*\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*)*\|?\s*$",line){return replace(line,r"^(\s*)",|_,g|format!("{}\\",g[0].as_deref().unwrap_or("")));}if index>0&&test(r"^ {0,3}-+\s*$",line){return replace(line,r"^(\s*)",|_,g|format!("{}\\",g[0].as_deref().unwrap_or("")));}let m=regex(r"^( {0,3})(?:(\d{1,9})(?=[.)](?:\s|$))|(#{1,6}(?=\s|$)|>|[-+*](?=\s|$)|-{3,}\s*$|=+\s*$))").try_exec(line).expect("Markdown regex");let Some(m)=m else{return line.into()};let indent=m.get(1).unwrap_or("");if let Some(n)=m.get(2){return format!("{indent}{n}\\{}",&line[m.end()..]);}if m.get(3).is_some_and(|v|v.starts_with('='))&&index==0{return line.into()}format!("{indent}\\{}",&line[indent.len()..])}
fn fence(s:&str,minimum:usize)->String{let mut longest=0;let mut current=0;for c in s.chars(){if c=='`'{current+=1;longest=longest.max(current)}else{current=0}}"`".repeat(minimum.max(longest+1))}
pub(super) fn fence_code_block(s:&str,info:&str)->String{let f=fence(s,3);format!("{f}{info}\n{s}\n{f}")}
fn code_span(s:&str)->String{let f=fence(s,1);let pad=s.starts_with('`')||s.ends_with('`')||(s.starts_with(' ')&&s.ends_with(' ')&&!s.trim().is_empty());format!("{f}{}{s}{}{f}",if pad{" "}else{""},if pad{" "}else{""})}
fn link(s:&str)->Option<String>{let s=s.trim();if !test(r"^(?:https?|mailto|ftp|tel):", &s.to_ascii_lowercase()){return None}Some(s.chars().map(|c|if matches!(c,' '|'('|')'|'<'|'>'){format!("%{:02X}",c as u32)}else{c.to_string()}).collect())}
fn formula(s:&str)->Option<String>{let mut s=replace(s,r"\s*\n\s*",|_,_|" ".into()).trim().to_owned();if s.is_empty()||s.contains('$'){return None}if s.ends_with('\\'){s.push_str("{}");}if test(r"^\d[\d.,]*(?:\s+[\p{L}\p{N}]+)*\s+\d[\d.,]*$",&s){s=format!("{{}}{s}");}Some(format!("${s}$"))}
fn marked(m:&Marks,k:u8)->bool{match k{0=>m.href.is_some(),1=>m.bold,_=>m.italic}}
fn open(k:u8)->&'static str{match k{0=>"[",1=>"**",_=>"*"}}
fn solid(s:&Seg)->bool{s.kind==Kind::Text&&!s.text.trim().is_empty()}
pub(super) fn render(runs:&[IpcValue],mode:Mode,host:&mut dyn Host,italic:bool)->Result<String,JobFailure>{
 let mut plain=String::new();for r in runs{match string(r,"kind"){"text"=>plain.push_str(string(r,"text")),"lineBreak"=>plain.push('\n'),_=>{}}}let brackets=risky(&plain);let dollars=runs.iter().any(|r|string(r,"kind")=="math")||math_syntax(&plain);let esc=|s:&str|escape(s,brackets,dollars,mode==Mode::Table);
 let mut segs:Vec<Seg>=Vec::new();for r in runs{match string(r,"kind"){
 "text"=>{let text=replace(string(r,"text"),r"[ \t]*[\r\n]+[ \t]*",|_,_|" ".into());if text.is_empty(){continue}let marks=Marks{bold:flag(r,"bold"),italic:flag(r,"italic"),href:link(string(r,"href"))};if let Some(last)=segs.last_mut(){if last.kind==Kind::Text&&last.marks==marks{last.text.push_str(&text);continue;}}segs.push(Seg{kind:Kind::Text,text,marks});},
 "lineBreak"=>segs.push(Seg{kind:if mode==Mode::Heading{Kind::Text}else{Kind::Break},text:if mode==Mode::Heading{" ".into()}else{String::new()},marks:Marks::default()}),
 "math"=>{let text=string(r,"latex");if let Some(last)=segs.last_mut(){if last.kind==Kind::Math{last.text.push(' ');last.text.push_str(text);continue;}}segs.push(Seg{kind:Kind::Math,text:text.into(),marks:Marks::default()});},
 "footnoteRef"=>{let id=string(r,"id");if let Some(n)=host.footnote_number(id){segs.push(Seg{kind:Kind::Footnote,text:format!("[^{n}]"),marks:Marks::default()})}else{host.missing_footnote(id);let marker=string(r,"marker");if !marker.is_empty(){segs.push(Seg{kind:Kind::Marker,text:esc(marker),marks:Marks::default()});}}},_=>return Err(JobFailure::new("failed",Some("unknown PDF inline run")))}}
 for s in &mut segs{if s.kind==Kind::Math{let value=formula(&s.text).unwrap_or_else(||{host.formula_as_code(&s.text);code_span(replace(&s.text,r"\s*\n\s*",|_,_|" ".into()).trim())});s.text=if mode==Mode::Table{value.replace('|',"\\|")}else{value};}}
 let solids=segs.iter().map(solid).collect::<Vec<_>>();for i in 0..segs.len(){if !solids[i]{let prev=(0..i).rev().find(|&j|solids[j]).map(|j|&segs[j].marks);let next=(i+1..segs.len()).find(|&j|solids[j]).map(|j|&segs[j].marks);let mut m=intersection(prev,next);if segs[i].kind==Kind::Footnote{m.href=None}segs[i].marks=m;}if italic{segs[i].marks.italic=true;}}
 let mut trimmed:Vec<Seg>=Vec::new();for s in segs{if s.kind==Kind::Break&&(trimmed.is_empty()||trimmed.last().is_some_and(|s|s.kind==Kind::Break)){continue}trimmed.push(s)}while trimmed.last().is_some_and(|s|s.kind==Kind::Break){trimmed.pop();}for i in 0..trimmed.len(){if trimmed[i].kind==Kind::Text{let start=i==0||trimmed[i-1].kind==Kind::Break;let end=i+1==trimmed.len()||trimmed[i+1].kind==Kind::Break;if start{trimmed[i].text=trimmed[i].text.trim_start().into()}if end{trimmed[i].text=trimmed[i].text.trim_end().into()}}}trimmed.retain(|s|s.kind!=Kind::Text||!s.text.is_empty());
 // CommonMark emphasis boundaries cannot leave punctuation adjacent to a word.
 let mut peeled:Vec<Seg>=Vec::new();for mut s in trimmed{if let Some(prev)=peeled.last_mut(){if s.kind==Kind::Text&&prev.kind==Kind::Text&&!prev.text.ends_with(char::is_whitespace)&&!s.text.starts_with(char::is_whitespace){let shared=intersection(Some(&prev.marks),Some(&s.marks));let opens=shared!=s.marks;let closes=shared!=prev.marks;let tail=regex(r"[\p{P}\p{S}]+$").try_exec(&prev.text).expect("Markdown regex").map(|m|(m.start(),m.as_str().to_owned()));let head=regex(r"^[\p{P}\p{S}]+").try_exec(&s.text).expect("Markdown regex").map(|m|m.as_str().to_owned());let close_emphasis=(prev.marks.bold&&!s.marks.bold)||(prev.marks.italic&&!s.marks.italic);let open_emphasis=(s.marks.bold&&!prev.marks.bold)||(s.marks.italic&&!prev.marks.italic);if close_emphasis&&!opens&&s.text.chars().next().is_some_and(|c|c.is_alphanumeric()){if let Some((at,tail))=tail{prev.text.truncate(at);if prev.text.is_empty(){peeled.pop();}peeled.push(Seg{kind:Kind::Text,text:tail,marks:shared});peeled.push(s);continue;}}if open_emphasis&&!closes&&prev.text.chars().last().is_some_and(|c|c.is_alphanumeric()){if let Some(head)=head{s.text=s.text[head.len()..].into();peeled.push(Seg{kind:Kind::Text,text:head,marks:shared});if !s.text.is_empty(){peeled.push(s)}continue;}}}}peeled.push(s);}
 let mut segs:Vec<Seg>=Vec::new();for s in peeled{if let Some(last)=segs.last_mut(){if s.kind==Kind::Text&&last.kind==Kind::Text&&s.marks==last.marks{last.text.push_str(&s.text);continue}}segs.push(s)}
 let mut out=String::new();let mut stack:Vec<(u8,Option<String>)>=Vec::new();let mut pending=String::new();let mut footnote_at_start=false;for i in 0..segs.len(){let s=&segs[i];let target=&s.marks;let mut keep=0;while keep<stack.len()&&marked(target,stack[keep].0)&&(stack[keep].0!=0||stack[keep].1==target.href){keep+=1}let mut between=String::new();for (mark,href) in stack.drain(keep..).rev(){if mark==0{between.push_str(&format!("]({})",href.unwrap_or_default()))}else{between.push_str(open(mark));}}
 let mut opening=(0..3).filter(|&m|marked(target,m)&&!stack.iter().any(|v|v.0==m)).map(|m|{let n=segs[i..].iter().take_while(|v|marked(&v.marks,m)&&(m!=0||v.marks.href==target.href)).count();(m,n)}).collect::<Vec<_>>();opening.sort_by(|a,b|b.1.cmp(&a.1));let (lead,mut core,trail)=if s.kind==Kind::Text{let t=s.text.trim_start();let lead=&s.text[..s.text.len()-t.len()];let core=t.trim_end();(lead.to_owned(),esc(core),t[core.len()..].to_owned())}else if s.kind==Kind::Break{(String::new(),if mode==Mode::Table{"<br>".into()}else{"  \n".into()},String::new())}else{(String::new(),s.text.clone(),String::new())};between.push_str(&esc(&format!("{pending}{lead}")));
 if between.is_empty()&&opening.is_empty()&&i>0{let prev=&segs[i-1];if prev.kind==Kind::Footnote&&s.kind==Kind::Text&&core.starts_with(|c|c=='('||c=='['||(footnote_at_start&&c==':')){core.insert(0,'\\');}if prev.kind==Kind::Math&&s.kind==Kind::Text&&core.chars().next().is_some_and(|c|c.is_ascii_digit()){let ch=core.remove(0);core=format!("&#{};{core}",ch as u32);}}
 let bracket=opening.first().is_some_and(|x|x.0==0)||(opening.is_empty()&&s.kind==Kind::Footnote);if between.is_empty()&&bracket&&i>0&&segs[i-1].kind==Kind::Text&&test(r"(?<!\\)(?:\\\\)*[!\]]$",&out){out.insert(out.len()-1,'\\');}out.push_str(&between);for (mark,_) in opening{out.push_str(open(mark));stack.push((mark,if mark==0{target.href.clone()}else{None}));}if s.kind==Kind::Footnote{footnote_at_start=out.is_empty()||out.ends_with('\n');}out.push_str(&core);pending=trail;}
 for (mark,href) in stack.into_iter().rev(){if mark==0{out.push_str(&format!("]({})",href.unwrap_or_default()))}else{out.push_str(open(mark));}}Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_entities_without_double_escaping_markup() {
        assert_eq!(escape("<x> &copy;",false,false,false),"&lt;x> &amp;copy;");
    }
    #[test]
    fn preserves_paragraph_markers_and_fences() {
        assert_eq!(escape_line_start("2. item",0),"2\\. item");
        assert_eq!(fence_code_block("```","latex"),"````latex\n```\n````");
    }
}
