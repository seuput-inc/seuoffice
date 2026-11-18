use std::collections::{HashMap, HashSet};
use jscompat::TryJsRegex;
use super::xlsx::numeric;

const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;
const MAX_CELLS: usize = 1_000_000;
const MAX_TABLES: usize = 64;
#[derive(Debug, Clone)]
pub(super) struct Sheet { pub name:String, pub rows:Vec<Vec<String>>, pub merges:Vec<(usize,usize,usize,usize)> }
struct RawCell { text:String, colspan:usize, rowspan:usize, number:Option<String> }
#[derive(Default)]
struct RawTable { rows:Vec<Vec<RawCell>> }
struct PendingCell { text:String, colspan:usize, rowspan:usize, number:Option<String> }
struct Tag { name:String, closing:bool, self_closing:bool, attrs:HashMap<String,String>, end:usize }

pub(super) fn entities(text:&str)->String {
    let mut out=String::with_capacity(text.len());let mut at=0;
    while let Some(relative)=text[at..].find('&') {
        let start=at+relative;out.push_str(&text[at..start]);
        let bytes=text.as_bytes();let mut end=start+1;
        if bytes.get(end)==Some(&b'#') {
            end+=1;let hex=matches!(bytes.get(end),Some(b'x'|b'X'));if hex{end+=1;}
            let begin=end;while bytes.get(end).is_some_and(|b|if hex{b.is_ascii_hexdigit()}else{b.is_ascii_digit()}){end+=1;}
            if end==begin{out.push('&');at=start+1;continue;}
        } else {
            if !bytes.get(end).is_some_and(u8::is_ascii_alphabetic){out.push('&');at=start+1;continue;}
            end+=1;while bytes.get(end).is_some_and(u8::is_ascii_alphanumeric){end+=1;}
        }
        if bytes.get(end)!=Some(&b';'){out.push('&');at=start+1;continue;}
        let body=&text[start+1..end];
        let value=if let Some(hex)=body.strip_prefix("#x").or_else(||body.strip_prefix("#X")) {u32::from_str_radix(hex,16).ok().and_then(char::from_u32).filter(|c|*c!='\0')}
        else if let Some(dec)=body.strip_prefix('#') {dec.parse::<u32>().ok().and_then(char::from_u32).filter(|c|*c!='\0')}
        else { match body.to_ascii_lowercase().as_str() {"amp"=>Some('&'),"lt"=>Some('<'),"gt"=>Some('>'),"quot"=>Some('"'),"apos"=>Some('\''),"nbsp"=>Some('\u{a0}'),"ensp"|"emsp"|"thinsp"=>Some(' '),"middot"=>Some('·'),"bull"=>Some('•'),"hellip"=>Some('…'),"ndash"=>Some('–'),"mdash"=>Some('—'),"lsquo"=>Some('‘'),"rsquo"=>Some('’'),"ldquo"=>Some('“'),"rdquo"=>Some('”'),"copy"=>Some('©'),"reg"=>Some('®'),"deg"=>Some('°'),"plusmn"=>Some('±'),"times"=>Some('×'),"divide"=>Some('÷'),"euro"=>Some('€'),"pound"=>Some('£'),"yen"=>Some('¥'),"cent"=>Some('¢'),"sect"=>Some('§'),"para"=>Some('¶'),_=>None} };
        if let Some(ch)=value {out.push(ch);} else {out.push_str(&text[start..=end]);} at=end+1;
    }
    out.push_str(&text[at..]);out
}
fn char_at(s:&str,i:usize)->Option<char> {s.get(i..)?.chars().next()}
fn tag_step(s:&str,i:&mut usize,units:&mut usize)->Option<char> {
    let c=char_at(s,*i)?;*units+=c.len_utf16();if *units>65536{return None;}*i+=c.len_utf8();Some(c)
}
fn tag(html:&str,start:usize)->Option<Tag> {
    let mut i=start+1;let mut units=1;let closing=char_at(html,i)==Some('/');if closing{tag_step(html,&mut i,&mut units)?;}
    if !char_at(html,i)?.is_ascii_alphabetic(){return None;}
    let name_start=i;
    while let Some(c)=char_at(html,i) {if jscompat::is_js_space(c)||matches!(c,'/'|'<'|'>'){break;}tag_step(html,&mut i,&mut units)?;}
    let name=html[name_start..i].to_ascii_lowercase();let mut attrs=HashMap::new();
    loop {
        while char_at(html,i).is_some_and(jscompat::is_js_space) {tag_step(html,&mut i,&mut units)?;}
        match char_at(html,i)? {
            '>'=>{tag_step(html,&mut i,&mut units)?;return Some(Tag{name,closing,self_closing:false,attrs,end:i});},
            '<'=>return None,
            '/' if html.as_bytes().get(i+1)==Some(&b'>')=>{tag_step(html,&mut i,&mut units)?;tag_step(html,&mut i,&mut units)?;return Some(Tag{name,closing,self_closing:true,attrs,end:i});},
            '/'=>{tag_step(html,&mut i,&mut units)?;continue;},
            _=>{}
        }
        let begin=i;while let Some(c)=char_at(html,i) {if jscompat::is_js_space(c)||matches!(c,'='|'/'|'>'|'<'){break;}tag_step(html,&mut i,&mut units)?;}
        let key=html[begin..i].to_ascii_lowercase();while char_at(html,i).is_some_and(jscompat::is_js_space){tag_step(html,&mut i,&mut units)?;}
        let mut value="";
        if char_at(html,i)==Some('=') {
            tag_step(html,&mut i,&mut units)?;while char_at(html,i).is_some_and(jscompat::is_js_space){tag_step(html,&mut i,&mut units)?;}
            match char_at(html,i) {
                Some(q @ ('\''|'"'))=>{tag_step(html,&mut i,&mut units)?;let begin=i;while char_at(html,i)?!=q {tag_step(html,&mut i,&mut units)?;}value=&html[begin..i];tag_step(html,&mut i,&mut units)?;},
                _=>{let begin=i;while let Some(c)=char_at(html,i){if jscompat::is_js_space(c)||matches!(c,'>'|'<'){break;}tag_step(html,&mut i,&mut units)?;}value=&html[begin..i];}
            }
        }
        if !key.is_empty(){attrs.insert(key,entities(value));}
    }
}
fn span(tag:&Tag,key:&str)->usize {
    let Some(raw)=tag.attrs.get(key) else{return 1};let raw=raw.trim_start_matches(jscompat::is_js_space);let raw=raw.strip_prefix('+').unwrap_or(raw);let digits=raw.bytes().take_while(u8::is_ascii_digit).count();
    if digits==0{return 1;}raw[..digits].parse::<usize>().unwrap_or(MAX_COLS).clamp(1,MAX_COLS)
}
fn limit_utf16(text:&str,units:usize)->String {
    let mut result=String::new();let mut used=0;
    for c in text.chars(){let width=c.len_utf16();if used+width>units{if used<units{result.push('\u{fffd}');}break;}result.push(c);used+=width;}result
}
fn normalize(raw:&str)->String {
    let decoded=entities(raw).replace('\u{a0}'," ");
    let lines:Vec<String>=decoded.split('\0').map(|line|line.split(jscompat::is_js_space).filter(|s|!s.is_empty()).collect::<Vec<_>>().join(" ")).collect();
    let first=lines.iter().position(|line|!line.is_empty()).unwrap_or(lines.len());let last=lines.iter().rposition(|line|!line.is_empty()).map_or(first,|i|i+1);
    let mut result=String::new();let mut blanks=0;
    for line in &lines[first..last] {if !result.is_empty(){if line.is_empty(){blanks+=1;if blanks<=1{result.push('\n');}}else {result.push('\n');blanks=0;}}result.push_str(line);}
    limit_utf16(&result,32767)
}
fn block(name:&str)->bool {matches!(name,"address"|"article"|"blockquote"|"caption"|"dd"|"div"|"dl"|"dt"|"figcaption"|"figure"|"footer"|"h1"|"h2"|"h3"|"h4"|"h5"|"h6"|"header"|"li"|"main"|"nav"|"ol"|"p"|"pre"|"section"|"ul")}
fn append(cell:&mut Option<PendingCell>,value:&str){if let Some(cell)=cell{cell.text.push_str(value);}}
fn finish_cell(cell:&mut Option<PendingCell>,row:&mut Option<Vec<RawCell>>,raw_cells:&mut usize)->Result<(),String> {
    if let Some(cell)=cell.take(){*raw_cells+=1;if *raw_cells>MAX_CELLS{return Err(format!("HTML import rejected: more than {MAX_CELLS} cells"));}row.get_or_insert_with(Vec::new).push(RawCell{text:normalize(&cell.text),colspan:cell.colspan,rowspan:cell.rowspan,number:cell.number});}Ok(())
}
fn finish_row(row:&mut Option<Vec<RawCell>>,table:&mut Option<RawTable>){if let Some(row)=row.take(){if let Some(table)=table{table.rows.push(row);}}}
fn layout(raw:RawTable)->Result<Option<Sheet>,String> {
    let mut rows:Vec<Vec<String>>=Vec::new();let mut occupied:HashMap<usize,HashSet<usize>>=HashMap::new();let mut merges=Vec::new();let mut slots=0usize;let mut last=None;
    for (r,raw_row) in raw.rows.into_iter().enumerate(){let mut c=0;
        for value in raw_row {while occupied.get(&r).is_some_and(|set|set.contains(&c)){c+=1;}
            if r>=MAX_ROWS || value.rowspan>MAX_ROWS-r{return Err(format!("HTML import rejected: more than {MAX_ROWS} rows"));}
            if c>=MAX_COLS || value.colspan>MAX_COLS-c{return Err(format!("HTML import rejected: more than {MAX_COLS} columns"));}
            slots=slots.checked_add(value.colspan*value.rowspan).ok_or("HTML import rejected: too many cells")?;if slots>MAX_CELLS{return Err(format!("HTML import rejected: more than {MAX_CELLS} cells"));}
            let value_text=value.number.as_deref().map(|s|s.trim_matches(jscompat::is_js_space)).filter(|s|numeric(s).is_some()).unwrap_or(&value.text);
            if !value_text.is_empty(){if rows.len()<=r{rows.resize_with(r+1,Vec::new);}if rows[r].len()<=c{rows[r].resize(c+1,String::new());}rows[r][c]=value_text.to_owned();}
            for rr in r..r+value.rowspan {let set=occupied.entry(rr).or_default();for cc in c..c+value.colspan{set.insert(cc);}}
            if value.colspan>1 || value.rowspan>1{merges.push((r,c,r+value.rowspan-1,c+value.colspan-1));}
            last=Some(last.map_or(r+value.rowspan-1,|old:usize|old.max(r+value.rowspan-1)));c+=value.colspan;
        }
        last=Some(last.map_or(r,|old|old.max(r)));
    }
    let Some(last)=last else{return Ok(None)};rows.resize_with(last+1,Vec::new);Ok(Some(Sheet{name:String::new(),rows,merges}))
}
fn names(html:&str)->Result<Vec<String>,String> {
    let re=TryJsRegex::new(r"<x:name[^>]*>([\s\S]*?)</x:name>","gi").map_err(|e|e.to_string())?;
    let tags=TryJsRegex::new(r"<[^>]*>","g").map_err(|e|e.to_string())?;let mut names=Vec::new();
    for m in re.try_find_iter(html){let m=m.map_err(|e|e.to_string())?;let decoded=entities(m.get(1).unwrap_or(""));let plain=tags.try_replace_all(&decoded, |_| String::new()).map_err(|e|e.to_string())?;let name=plain.trim_matches(jscompat::is_js_space);if !name.is_empty(){names.push(name.to_owned());}}Ok(names)
}
fn clean_name(name:&str)->Option<String> {
    let clean:String=name.chars().filter(|c|!matches!(c,'\\'|'/'|'?'|'*'|'['|']'|':')).collect();let clean=clean.trim_matches('\'').trim_matches(jscompat::is_js_space);
    let mut out=String::new();let mut units=0;for c in clean.chars(){if units+c.len_utf16()>31{break;}out.push(c);units+=c.len_utf16();}(!out.is_empty()).then_some(out)
}
fn name_prefix(text:&str,max:usize)->String {
    let mut out=String::new();let mut count=0;for c in text.chars(){if count+c.len_utf16()>max{break;}out.push(c);count+=c.len_utf16();}out
}
fn skip_raw(html:&str,lower:&str,from:usize,name:&str)->usize {
    let needle=format!("</{name}");let mut at=from;
    while let Some(relative)=lower[at..].find(&needle) {
        let start=at+relative;let next=start+needle.len();
        if char_at(html,next).is_some_and(|c|c=='>' || jscompat::is_js_space(c)) {
            return html[next..].find('>').map_or(html.len(),|n|next+n+1);
        }
        at=next;
    }
    html.len()
}
pub(super) fn parse(html:&str)->Result<Vec<Sheet>,String> {
    let mut raw=Vec::new();let mut depth=0usize;let mut table=None;let mut row=None;let mut cell=None;let mut text_start=0usize;let mut index=0usize;let mut raw_cells=0usize;let lower=html.to_ascii_lowercase();
    while let Some(rel)=html[index..].find('<') {let start=index+rel;if start>text_start{append(&mut cell,&html[text_start..start]);text_start=start;}
        if html[start..].starts_with("<!--"){index=html[start+4..].find("-->").map_or(html.len(),|n|start+4+n+3);text_start=index;continue;}
        if html[start..].starts_with("<![CDATA["){let begin=start+9;let end=html[begin..].find("]]>").map_or(html.len(),|n|begin+n);append(&mut cell,&html[begin..end]);index=if end==html.len(){end}else{end+3};text_start=index;continue;}
        if html[start..].starts_with("<!")||html[start..].starts_with("<?"){index=html[start+2..].find('>').map_or(html.len(),|n|start+2+n+1);text_start=index;continue;}
        let Some(t)=tag(html,start) else {index=start+1;continue};index=t.end;text_start=index;
        if !t.closing && !t.self_closing && matches!(t.name.as_str(),"script"|"style") {index=skip_raw(html,&lower,index,&t.name);text_start=index;continue;}
        match t.name.as_str() {
            "table"=>if !t.closing {if depth==0{table=Some(RawTable::default());row=None;cell=None;}else{append(&mut cell," ");}depth+=1;}
                else if depth>0 {if depth==1{finish_cell(&mut cell,&mut row,&mut raw_cells)?;finish_row(&mut row,&mut table);if let Some(t)=table.take(){raw.push(t);}}else{append(&mut cell," ");}depth-=1;},
            "tr"=>if depth==1 {finish_cell(&mut cell,&mut row,&mut raw_cells)?;finish_row(&mut row,&mut table);if !t.closing{row=Some(Vec::new());}}else{append(&mut cell," ");},
            "td"|"th"=>if depth==1 {finish_cell(&mut cell,&mut row,&mut raw_cells)?;if !t.closing {if row.is_none(){row=Some(Vec::new());}cell=Some(PendingCell{text:String::new(),colspan:span(&t,"colspan"),rowspan:span(&t,"rowspan"),number:t.attrs.get("x:num").cloned()});}}else{append(&mut cell," ");},
            "br"=>append(&mut cell,"\0"),
            "img"=>if let Some(alt)=t.attrs.get("alt").map(|v|v.trim_matches(jscompat::is_js_space)).filter(|v|!v.is_empty()){append(&mut cell,alt)},
            other=>if block(other){append(&mut cell,"\0")},
        }
        if raw.len()>MAX_TABLES{return Err(format!("HTML import rejected: more than {MAX_TABLES} tables"));}
    }
    finish_cell(&mut cell,&mut row,&mut raw_cells)?;finish_row(&mut row,&mut table);if let Some(t)=table{raw.push(t);}if raw.len()>MAX_TABLES{return Err(format!("HTML import rejected: more than {MAX_TABLES} tables"));}
    let mut layouts=Vec::new();for t in raw {if let Some(t)=layout(t)?{layouts.push(t);}}if layouts.is_empty(){return Ok(Vec::new());}
    let names=names(html)?;let cleaned:Vec<_>=if names.len()==layouts.len(){names.iter().map(|n|clean_name(n)).collect()}else{vec![None;layouts.len()]};
    if cleaned.iter().any(Option::is_none) {let name=if names.len()==1{clean_name(&names[0]).unwrap_or_else(||"Sheet1".into())}else{"Sheet1".into()};let mut result=Sheet{name,rows:Vec::new(),merges:Vec::new()};for sheet in layouts {if !result.rows.is_empty(){result.rows.push(Vec::new());}let offset=result.rows.len();result.rows.extend(sheet.rows);result.merges.extend(sheet.merges.into_iter().map(|(r,c,r2,c2)|(r+offset,c,r2+offset,c2)));}return Ok(vec![result]);}
    let mut used=HashSet::new();for (sheet,name) in layouts.iter_mut().zip(cleaned){let base=name.expect("all sheet names validated");let mut name=base.clone();let mut suffix=2usize;while used.contains(&name){let tail=format!(" ({suffix})");let limit=31usize.saturating_sub(tail.encode_utf16().count());name=format!("{}{}",name_prefix(&base,limit),tail);suffix+=1;}used.insert(name.clone());sheet.name=name;}Ok(layouts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_tables_entities_breaks_and_spans_become_one_grid() {
        let html="<table><tr><td rowspan='2'> A&nbsp;B<br>next <table><tr><td>nested</td></tr></table></td><td x:num='1234.5'>1,234.50</td></tr><tr><td colspan='2'>tail &amp; end</td></tr></table>";
        let sheets=parse(html).unwrap();
        assert_eq!(sheets.len(),1);assert_eq!(sheets[0].name,"Sheet1");
        assert_eq!(sheets[0].rows[0],vec!["A B\nnext nested","1234.5"]);
        assert_eq!(sheets[0].rows[1],vec!["","tail & end"]);
        assert_eq!(sheets[0].merges,vec![(0,0,1,0),(1,1,1,2)]);
    }
    #[test]
    fn excel_sheet_names_and_duplicate_suffix_are_preserved() {
        let html="<!--[if gte mso 9]><x:name>Data</x:name><x:name>Data</x:name><![endif]--><table><tr><td>one</td></tr></table><table><tr><td>two</td></tr></table>";
        let sheets=parse(html).unwrap();assert_eq!(sheets.len(),2);
        assert_eq!(sheets[0].name,"Data");assert_eq!(sheets[1].name,"Data (2)");
        assert_eq!(sheets[1].rows[0][0],"two");
    }
    #[test]
    fn unnamed_tables_stack_and_shift_merges() {
        let sheets=parse("<table><tr><td>a</td></tr></table><table><tr><td colspan='2'>b</td></tr></table>").unwrap();
        assert_eq!(sheets.len(),1);assert_eq!(sheets[0].rows.len(),3);assert_eq!(sheets[0].merges,vec![(2,0,2,1)]);
    }
    #[test]
    fn filters_script_and_bounds_spans() {
        let sheets=parse("<table><tr><td>a<script>bad </scriptbad> worse </script><img alt='&amp;'/></td></tr></table>").unwrap();
        assert_eq!(sheets[0].rows[0][0],"a&");
        assert!(parse("<table><tr><td rowspan='9999999' colspan='16384'>x</td></tr></table>").unwrap_err().contains("cells"));
        assert_eq!(entities("&bad &amp; &#x1F600; &bogus;"),"&bad & 😀 &bogus;");
    }
}
