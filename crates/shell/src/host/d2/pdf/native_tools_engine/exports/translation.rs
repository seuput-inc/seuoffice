//! Translation derivation and stored AI readings for PDF structure trees.
use std::collections::HashMap;
use std::path::Path;
use shell_contract::IpcValue as V;
use shell_ipc_d2::pdf::tools::{JobFailure, StoreMethod};
use main_core::packages::pdf_structure::{keys::{translation_block_keys, KeyUnit}, tree_inline::InlineRun};

mod readings;
fn field<'a>(v:&'a V,key:&str)->&'a V { if let V::Object(o)=v {o.iter().find(|(k,_)|k==key).map(|(_,v)|v).unwrap_or(&V::Undefined)}else{&V::Undefined} }
fn text(v:&V)->&str{if let V::String(s)=v{s}else{""}}
fn s<'a>(v:&'a V,key:&str)->&'a str{text(field(v,key))}
fn arr(v:&V)->&[V]{if let V::Array(v)=v{v}else{&[]}}
fn a<'a>(v:&'a V,key:&str)->&'a[V]{arr(field(v,key))}
fn set(v:&mut V,key:&str,value:V){if let V::Object(fields)=v {if let Some((_,v))=fields.iter_mut().find(|(k,_)|k==key){*v=value}else{fields.push((key.into(),value));}}}
fn remove(v:&mut V,key:&str){if let V::Object(fields)=v {fields.retain(|(k,_)|k!=key);}}
fn string(s:impl Into<String>)->V{V::String(s.into())}
fn obj(fields:Vec<(&str,V)>)->V{V::Object(fields.into_iter().map(|(k,v)|(k.into(),v)).collect())}
fn plain(t:&str)->V{V::Array(vec![obj(vec![("kind",string("text")),("text",string(t))])])}
fn mark(v:&V,lang:&str)->V {let mut out=v.clone();let id=s(v,"id");set(&mut out,"id",string(format!("{id}@{lang}")));set(&mut out,"translationOf",string(id));set(&mut out,"lang",string(lang));out}
fn cell_key(id:&str,c:&V,k:usize)->String {format!("{}#{}.{}.{}",id,num(field(c,"row")),num(field(c,"col")),k)}
fn num(v:&V)->i64{if let V::Number(n)=v{*n as i64}else{0}}
fn caption(c:&V,lang:&str,tr:&HashMap<String,V>)->Option<V>{let runs=tr.get(s(c,"id"))?;let mut out=mark(c,lang);set(&mut out,"runs",runs.clone());Some(out)}
fn block(b:&V,lang:&str,tr:&HashMap<String,V>)->Option<V>{
    match s(b,"kind") {
        "heading"|"paragraph"|"listItem"=>{let r=tr.get(s(b,"id"))?;let mut out=mark(b,lang);set(&mut out,"runs",r.clone());Some(out)}
        "image"=>{let c=caption(field(b,"caption"),lang,tr)?;let mut out=mark(b,lang);set(&mut out,"caption",c);Some(out)}
        "table"=>{
            let table=field(b,"table");let mut any=false;let mut cells=Vec::new();
            for c in a(table,"cells") {let mut new=c.clone();let mut pars=Vec::new();for (k,p) in a(c,"paragraphs").iter().enumerate(){if let Some(t)=tr.get(&cell_key(s(b,"id"),c,k)){any=true;pars.push(t.clone())}else{pars.push(p.clone())}}set(&mut new,"paragraphs",V::Array(pars));cells.push(new);}
            let cap=caption(field(b,"caption"),lang,tr);if !any&&cap.is_none(){return None;}
            let mut new_table=table.clone();set(&mut new_table,"cells",V::Array(cells));let mut out=mark(b,lang);set(&mut out,"table",new_table);if let Some(cap)=cap{set(&mut out,"caption",cap)}else{remove(&mut out,"caption")}Some(out)
        }
        _=>None,
    }
}
fn derive(mut tree:V,lang:&str,tr:&HashMap<String,V>,bilingual:bool)->V{
    let mut blocks=Vec::new();for b in a(&tree,"blocks") {let t=block(b,lang,tr);if bilingual {if s(b,"kind")=="image" {let mut out=b.clone();if let Some(t)=t{set(&mut out,"captionTranslation",field(&t,"caption").clone());}blocks.push(out)}else{blocks.push(b.clone());if let Some(t)=t{blocks.push(t)}}}else{blocks.push(t.unwrap_or_else(||b.clone()));}}
    let mut notes=Vec::new();for n in a(&tree,"footnotes"){let mut note=n.clone();let mut bs=Vec::new();for p in a(n,"blocks"){let t=tr.get(s(p,"id")).map(|r|{let mut v=mark(p,lang);set(&mut v,"runs",r.clone());v});if bilingual{bs.push(p.clone());if let Some(t)=t{bs.push(t)}}else{bs.push(t.unwrap_or_else(||p.clone()));}}set(&mut note,"blocks",V::Array(bs));notes.push(note);}
    set(&mut tree,"variant",string(if bilingual{"bilingual"}else{"translated"}));set(&mut tree,"targetLanguage",string(lang));set(&mut tree,"blocks",V::Array(blocks));set(&mut tree,"footnotes",V::Array(notes));tree
}
struct Unit {key:String,block:String,page:V,fingerprint:String,runs:Vec<InlineRun>}
fn native_runs(value:&V)->Vec<InlineRun>{arr(value).iter().filter_map(|r|match s(r,"kind"){"text"=>Some(InlineRun::Text{text:s(r,"text").into()}),"lineBreak"=>Some(InlineRun::LineBreak),"math"=>Some(InlineRun::Math{latex:s(r,"latex").into()}),"footnoteRef"=>Some(InlineRun::FootnoteRef),_=>None}).collect()}
fn units(tree:&V)->Vec<Unit>{
    let mut out=Vec::new();let mut add=|key:String,r:&V,b:&V| {let runs=native_runs(r);if runs.iter().any(|r|matches!(r,InlineRun::Text{text} if jscompat::js_regex!(r"\p{L}","u").test(text))){out.push(Unit{key,block:s(b,"id").into(),page:field(b,"page").clone(),fingerprint:s(b,"pageFingerprint").into(),runs});}};
    for b in a(tree,"blocks"){let kind=s(b,"kind");if matches!(kind,"heading"|"paragraph"|"listItem"){add(s(b,"id").into(),field(b,"runs"),b)}if matches!(kind,"image"|"table"){let c=field(b,"caption");if !matches!(c,V::Undefined|V::Null){add(s(c,"id").into(),field(c,"runs"),c)}}if kind=="table" {for c in a(field(b,"table"),"cells"){for (k,p) in a(c,"paragraphs").iter().enumerate(){add(cell_key(s(b,"id"),c,k),p,b)}}}}
    for n in a(tree,"footnotes"){for p in a(n,"blocks"){add(s(p,"id").into(),field(p,"runs"),p)}}out
}

pub(super) struct WritebackUnit {
    pub key: String,
    pub block_id: String,
    pub page: V,
    pub runs: V,
}

pub(super) fn writeback_units(tree: &V) -> Vec<WritebackUnit> {
    let mut out=Vec::new();
    let mut add=|key:String,runs:&V,b:&V|out.push(WritebackUnit {key,block_id:s(b,"id").into(),page:field(b,"page").clone(),runs:runs.clone()});
    for b in a(tree,"blocks") {
        let kind=s(b,"kind");
        if matches!(kind,"heading"|"paragraph"|"listItem"){add(s(b,"id").into(),field(b,"runs"),b)}
        if matches!(kind,"image"|"table"){let c=field(b,"caption");if !matches!(c,V::Undefined|V::Null){add(s(c,"id").into(),field(c,"runs"),c)}}
        if kind=="table"{for c in a(field(b,"table"),"cells"){for(k,p) in a(c,"paragraphs").iter().enumerate(){add(cell_key(s(b,"id"),c,k),p,b)}}}
    }
    for n in a(tree,"footnotes"){for p in a(n,"blocks"){add(s(p,"id").into(),field(p,"runs"),p)}}
    out
}

pub(super) fn translated_runs(tree:&V,_lang:&str)->HashMap<String,V>{
    let mut out=HashMap::new();
    let mut add=|b:&V|{if let V::String(original)=field(b,"translationOf"){out.insert(original.clone(),field(b,"runs").clone());}};
    for b in a(tree,"blocks"){
        if matches!(s(b,"kind"),"heading"|"paragraph"|"listItem"){add(b)}
        for key in ["caption","captionTranslation"]{add(field(b,key));}
    }
    for n in a(tree,"footnotes"){for p in a(n,"blocks"){add(p)}}
    drop(add);
    for b in a(tree,"blocks"){
        if s(b,"kind")!="table"{continue;}
        let V::String(original)=field(b,"translationOf") else{continue};
        for c in a(field(b,"table"),"cells"){for(k,p) in a(c,"paragraphs").iter().enumerate(){out.insert(cell_key(original,c,k),p.clone());}}
    }
    out
}
fn collect(tree:&V,lang:&str,stored:&V)->(HashMap<String,V>,V){
    let units=units(tree);let key_inputs=units.iter().map(|u|KeyUnit{runs:&u.runs,page_fingerprint:&u.fingerprint}).collect::<Vec<_>>();let keys=translation_block_keys(&key_inputs);
    let mut translations=HashMap::new();let mut untranslated=Vec::new();let mut pending=0;
    for (unit,key) in units.iter().zip(keys){let entry=a(stored,"blocks").iter().find(|e|{let k=field(e,"key");s(k,"pageFingerprint")==key.page_fingerprint&&s(k,"textHash")==key.text_hash&&matches!(field(k,"occurrence"),V::Number(n) if *n==key.occurrence)});
        match entry {None=>pending+=1,Some(e) if s(e,"status")=="translated"&&!matches!(field(e,"text"),V::Undefined)=>{let runs=if matches!(field(e,"edited"),V::Bool(true)){plain(s(e,"text"))}else{match field(e,"runs"){V::Undefined|V::Null=>plain(s(e,"text")),v=>v.clone()}};translations.insert(unit.key.clone(),runs);},Some(e)=>{let reason=match field(e,"reason"){V::Undefined|V::Null=>"unknown",v=>text(v)};untranslated.push(obj(vec![("unitKey",string(&unit.key)),("blockId",string(&unit.block)),("page",unit.page.clone()),("reason",string(reason))]));}}
    }
    let total=units.len();let report=obj(vec![("lang",string(lang)),("total",V::Number(total as f64)),("translated",V::Number(translations.len() as f64)),("untranslated",V::Array(untranslated)),("pending",V::Number(pending as f64)),("complete",V::Bool(pending==0)),("provider",match field(stored,"provider"){V::Undefined=>V::Null,v=>v.clone()}),("model",match field(stored,"model"){V::Undefined=>V::Null,v=>v.clone()})]);(translations,report)
}
pub(super) fn apply(tree:V,doc_hash:&str,input:Option<&V>,user_data:&Path)->Result<(V,Option<V>),JobFailure>{
    let Some(input)=input.filter(|v|!matches!(v,V::Null|V::Undefined)) else{return Ok((tree,None))};let lang=s(input,"lang");let bilingual=matches!(field(input,"bilingual"),V::Bool(true));
    if let V::Array(units)=field(input,"units"){let mut tr=HashMap::new();for u in units{tr.insert(s(u,"key").to_owned(),field(u,"runs").clone());}return Ok((derive(tree,lang,&tr,bilingual),None));}
    let stored=super::super::super::store::store(user_data.to_owned(),StoreMethod::ReadTranslation,vec![string(doc_hash),string(lang)]).map_err(|error|{let message=match error{shell_ipc_d2::pdf::tools::StoreError::Type(s)|shell_ipc_d2::pdf::tools::StoreError::Write(s)|shell_ipc_d2::pdf::tools::StoreError::Other(s)=>s};JobFailure::new("failed",Some(&message))})?;
    let (tr,report)=collect(&tree,lang,&stored);Ok((derive(tree,lang,&tr,bilingual),Some(report)))
}
pub(super) fn with_ai_readings(tree:V,doc_hash:&str,user_data:&Path)->Result<V,JobFailure>{
    if s(&tree,"variant")!="original" {return Ok(tree)}
    let entries=match super::super::super::store::store(user_data.to_owned(),StoreMethod::ReadAiReadPages,vec![string(doc_hash)]){Ok(v)=>v,Err(_)=>return Ok(tree)};
    readings::merge(tree,&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bilingual_retains_images_and_attaches_translated_caption() {
        let caption=obj(vec![("id",string("cap")),("runs",plain("original"))]);
        let image=obj(vec![("id",string("img")),("kind",string("image")),("data",V::Bytes(shell_contract::BytesKind::Uint8Array,vec![1,2])),("caption",caption)]);
        let tree=obj(vec![("variant",string("original")),("blocks",V::Array(vec![image])),("footnotes",V::Array(vec![]))]);
        let tr=HashMap::from([("cap".into(),plain("translated"))]);
        let result=derive(tree,"ko",&tr,true);
        assert_eq!(a(&result,"blocks").len(),1);
        let image=&a(&result,"blocks")[0];
        assert_eq!(field(image,"data"),&V::Bytes(shell_contract::BytesKind::Uint8Array,vec![1,2]));
        assert_eq!(s(field(image,"captionTranslation"),"translationOf"),"cap");
    }

    #[test]
    fn report_counts_untranslated_and_pending_units() {
        let paragraph=obj(vec![("id",string("p")),("kind",string("paragraph")),("page",V::Number(1.0)),("pageFingerprint",string("fingerprint")),("runs",plain("Hello"))]);
        let tree=obj(vec![("blocks",V::Array(vec![paragraph])),("footnotes",V::Array(vec![]))]);
        let (_,report)=collect(&tree,"ko",&V::Null);
        assert_eq!(field(&report,"pending"),&V::Number(1.0));
        assert_eq!(field(&report,"complete"),&V::Bool(false));
    }
}
