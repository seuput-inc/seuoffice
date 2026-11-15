//! Validated stored AI pages merged into the original tree.
use super::{a,arr,field,num,obj,s,set,string,text,V};
use shell_contract::BytesKind;
use shell_ipc_d2::pdf::tools::JobFailure;
use std::collections::{HashMap,HashSet};

fn int(v:&V,min:i64,max:i64)->bool{matches!(v,V::Number(n) if n.is_finite() && n.fract()==0.0 && *n>=min as f64 && *n<=max as f64)}
fn valid_runs(v:&V)->bool{let V::Array(runs)=v else{return false};runs.iter().all(|r|match s(r,"kind"){
    "text"=>matches!(field(r,"text"),V::String(_))&&["bold","italic"].iter().all(|k|matches!(field(r,k),V::Undefined|V::Bool(true)))&&matches!(field(r,"href"),V::Undefined|V::String(_)),
    "footnoteRef"=>matches!(field(r,"id"),V::String(id) if !id.is_empty()),"math"=>matches!(field(r,"latex"),V::String(_)),"lineBreak"=>true,_=>false})}
fn valid_caption(v:&V)->bool{matches!(v,V::Undefined)||matches!(s(v,"position"),"above"|"below")&&valid_runs(field(v,"runs"))}
fn valid_table(v:&V)->bool{let rows=num(field(v,"rowCount"));let cols=num(field(v,"colCount"));int(field(v,"rowCount"),0,i64::MAX)&&int(field(v,"colCount"),0,i64::MAX)&&matches!(field(v,"cells"),V::Array(_))&&a(v,"cells").iter().all(|c|{let row=num(field(c,"row"));let col=num(field(c,"col"));int(field(c,"row"),0,rows-1)&&int(field(c,"col"),0,cols-1)&&int(field(c,"rowSpan"),1,rows-row)&&int(field(c,"colSpan"),1,cols-col)&&matches!(field(c,"header"),V::Undefined|V::Bool(true))&&matches!(field(c,"paragraphs"),V::Array(_))&&a(c,"paragraphs").iter().all(valid_runs)})}
fn valid_payload(p:&V)->bool{
    matches!(field(p,"version"),V::Number(n) if *n==1.0)&&matches!(field(p,"blocks"),V::Array(_))&&matches!(field(p,"footnotes"),V::Array(_))&&int(field(p,"droppedRepetitions"),0,9_007_199_254_740_991)&&matches!(field(p,"reasked"),V::Bool(_))&&
    a(p,"blocks").iter().all(|b|match s(b,"kind"){
        "heading"=>int(field(b,"level"),1,6)&&valid_runs(field(b,"runs")),"paragraph"=>valid_runs(field(b,"runs")),
        "listItem"=>matches!(field(b,"ordered"),V::Bool(_))&&int(field(b,"level"),0,32)&&matches!(field(b,"marker"),V::Undefined|V::String(_))&&valid_runs(field(b,"runs")),
        "math"=>matches!(field(b,"latex"),V::String(_)),"table"=>valid_table(field(b,"table"))&&valid_caption(field(b,"caption")),"figure"=>valid_caption(field(b,"caption")),_=>false})&&
    a(p,"footnotes").iter().all(|n|matches!(field(n,"label"),V::String(label) if !label.is_empty())&&matches!(field(n,"paragraphs"),V::Array(_))&&a(n,"paragraphs").iter().all(valid_runs))
}
fn map_runs(runs:&V,ids:&HashMap<String,String>,page:i64,referenced:&mut HashSet<String>,count_refs:bool,unlinked:&mut bool)->V{
    V::Array(arr(runs).iter().map(|r|{
        let mut out=match s(r,"kind"){
            "text"=>{let mut v=obj(vec![("kind",string("text")),("text",field(r,"text").clone())]);for k in ["bold","italic","href"]{if !matches!(field(r,k),V::Undefined){set(&mut v,k,field(r,k).clone());}}v},
            "footnoteRef"=>{let label=s(r,"id");let id=if let Some(id)=ids.get(label){if count_refs{referenced.insert(label.into());}id.clone()}else{if count_refs{*unlinked=true;}format!("p{page}.fn-missing-{label}")};obj(vec![("kind",string("footnoteRef")),("id",string(id)),("marker",string(label))])},
            "math"=>obj(vec![("kind",string("math")),("latex",field(r,"latex").clone())]),_=>obj(vec![("kind",string("lineBreak"))]),
        }; let _=&mut out;out
    }).collect())
}
fn head(page:i64,fp:&str,n:&mut usize)->V{*n+=1;obj(vec![("id",string(format!("p{page}.ai{n}"))),("page",V::Number(page as f64)),("pageFingerprint",string(fp)),("source",string("ai"))])}
struct Reading{page:i64,fp:String,blocks:Vec<V>,notes:Vec<V>,failed:bool}
fn materialize(p:&V,page:i64,fp:&str)->Reading{
    let mut ids=HashMap::new();let mut labels=Vec::new();let mut paragraphs:HashMap<String,Vec<V>>=HashMap::new();
    for n in a(p,"footnotes"){let label=s(n,"label");if !ids.contains_key(label){labels.push(label.to_owned());ids.insert(label.into(),format!("p{page}.fn{}",labels.len()));}paragraphs.entry(label.into()).or_default().extend(a(n,"paragraphs").iter().cloned());}
    let mut referenced=HashSet::new();let mut failed=false;let mut next=0;let mut blocks=Vec::new();
    for b in a(p,"blocks"){
        let kind=s(b,"kind");let mut out=head(page,fp,&mut next);
        set(&mut out,"kind",string(if kind=="figure"{"image"}else{kind}));
        match kind {
            "heading"=>{set(&mut out,"level",field(b,"level").clone());set(&mut out,"levelSource",string("ai"));set(&mut out,"runs",map_runs(field(b,"runs"),&ids,page,&mut referenced,true,&mut failed));},
            "paragraph"=>set(&mut out,"runs",map_runs(field(b,"runs"),&ids,page,&mut referenced,true,&mut failed)),
            "listItem"=>{for k in ["ordered","level","marker"]{if !matches!(field(b,k),V::Undefined){set(&mut out,k,field(b,k).clone());}}set(&mut out,"runs",map_runs(field(b,"runs"),&ids,page,&mut referenced,true,&mut failed));},
            "math"=>set(&mut out,"latex",field(b,"latex").clone()),
            "table"|"figure"=>{
                let c=field(b,"caption");if !matches!(c,V::Undefined){let mut cap=head(page,fp,&mut next);set(&mut cap,"kind",string("caption"));set(&mut cap,"position",field(c,"position").clone());set(&mut cap,"runs",map_runs(field(c,"runs"),&ids,page,&mut referenced,true,&mut failed));set(&mut out,"caption",cap);}
                if kind=="figure"{set(&mut out,"data",V::Bytes(BytesKind::Uint8Array,vec![]));set(&mut out,"mime",string("image/png"));for k in ["pixelWidth","pixelHeight","widthPt","heightPt"]{set(&mut out,k,V::Number(0.0));}set(&mut out,"placeholder",V::Bool(true));}
                else {let table=field(b,"table");let mut cells=Vec::new();for c in a(table,"cells"){let mut cell=obj(vec![]);for k in ["row","col","rowSpan","colSpan","header"]{if !matches!(field(c,k),V::Undefined){set(&mut cell,k,field(c,k).clone());}}set(&mut cell,"paragraphs",V::Array(a(c,"paragraphs").iter().map(|p|map_runs(p,&ids,page,&mut referenced,true,&mut failed)).collect()));cells.push(cell);}set(&mut out,"table",obj(vec![("rowCount",field(table,"rowCount").clone()),("colCount",field(table,"colCount").clone()),("cells",V::Array(cells))]));}
            },_=>{}
        }blocks.push(out);
    }
    let mut notes=Vec::new();for label in labels{let id=&ids[&label];let mut bs=Vec::new();for (i,p) in paragraphs[&label].iter().enumerate(){let mut block=obj(vec![("id",string(format!("{id}.{}",i+1))),("page",V::Number(page as f64)),("pageFingerprint",string(fp)),("source",string("ai")),("kind",string("paragraph"))]);set(&mut block,"runs",map_runs(p,&ids,page,&mut referenced,false,&mut failed));bs.push(block);}if !referenced.contains(&label){failed=true;}notes.push(obj(vec![("id",string(id)),("marker",string(&label)),("page",V::Number(page as f64)),("pageFingerprint",string(fp)),("blocks",V::Array(bs))]));}
    Reading{page,fp:fp.into(),blocks,notes,failed}
}
fn refs(runs:&[V],out:&mut HashSet<String>){for r in runs{if s(r,"kind")=="footnoteRef"{out.insert(s(r,"id").into());}}}
fn block_refs(b:&V,out:&mut HashSet<String>){refs(a(b,"runs"),out);if s(b,"kind")=="table"{for c in a(field(b,"table"),"cells"){for p in a(c,"paragraphs"){refs(arr(p),out);}}}if matches!(s(b,"kind"),"table"|"image"){refs(a(field(b,"caption"),"runs"),out);}}
pub(super) fn merge(mut tree:V,entries:&V)->Result<V,JobFailure>{
    let pages=a(&tree,"pages");let mut readings=Vec::new();for p in pages{let fp=s(p,"fingerprint");if let Some(entry)=arr(entries).iter().find(|e|s(e,"pageFingerprint")==fp){let payload=field(entry,"payload");if valid_payload(payload){readings.push(materialize(payload,num(field(p,"page")),fp));}}}
    if readings.is_empty(){return Ok(tree)}
    let order:HashMap<i64,usize>=pages.iter().enumerate().map(|(i,p)|(num(field(p,"page")),i)).collect();
    let map:HashMap<i64,&Reading>=readings.iter().map(|r|(r.page,r)).collect();let mut blocks=Vec::new();let mut warnings=a(&tree,"warnings").to_vec();let mut placed=HashSet::new();
    for b in a(&tree,"blocks"){let page=num(field(b,"page"));if s(b,"kind")=="pageBreak"{blocks.push(b.clone());if let Some(r)=map.get(&page)&&placed.insert(page){blocks.extend(r.blocks.clone());}continue;}
        let regions=a(b,"regions");let ps=if regions.is_empty(){vec![page]}else{regions.iter().map(|r|num(field(r,"page"))).collect()};let replaced=ps.iter().filter(|p|map.contains_key(p)).copied().collect::<Vec<_>>();if !replaced.is_empty()&&replaced.len()==ps.len(){continue;}if !replaced.is_empty(){let others=ps.iter().filter(|p|!map.contains_key(p)).map(ToString::to_string).collect::<Vec<_>>().join(", ");for p in replaced{warnings.push(string(format!("page {p}: a block crossing to page {others} was kept beside the AI reading")));}}blocks.push(b.clone());
    }
    for r in &readings{if placed.contains(&r.page){continue;}let rank=order[&r.page];let at=blocks.iter().position(|b|s(b,"kind")=="pageBreak"&&order.get(&num(field(b,"page"))).is_some_and(|i|*i>rank)).unwrap_or(blocks.len());let boundary=obj(vec![("id",string(format!("p{}.ai0",r.page))),("page",V::Number(r.page as f64)),("pageFingerprint",string(&r.fp)),("source",string("ai")),("kind",string("pageBreak"))]);let mut inserted=vec![boundary];inserted.extend(r.blocks.clone());blocks.splice(at..at,inserted);}
    let mut referenced=HashSet::new();for b in &blocks{if s(b,"source")!="ai"{block_refs(b,&mut referenced)}}
    let mut notes=a(&tree,"footnotes").iter().filter(|n|!map.contains_key(&num(field(n,"page")))||referenced.contains(s(n,"id"))).cloned().collect::<Vec<_>>();for r in &readings{notes.extend(r.notes.clone());}notes.sort_by_key(|n|order.get(&num(field(n,"page"))).copied().unwrap_or(usize::MAX));
    let mut failed=a(&tree,"footnoteLinkFailedPages").iter().map(num).filter(|p|!map.contains_key(p)).collect::<Vec<_>>();for r in &readings{if r.failed&&!failed.contains(&r.page){failed.push(r.page);}}failed.sort();
    let pages=a(&tree,"pages").iter().map(|p|{let mut out=p.clone();if map.contains_key(&num(field(p,"page"))){set(&mut out,"status",string("ai"));}out}).collect();
    set(&mut tree,"pages",V::Array(pages));set(&mut tree,"blocks",V::Array(blocks));set(&mut tree,"footnotes",V::Array(notes));set(&mut tree,"footnoteLinkFailedPages",V::Array(failed.into_iter().map(|p|V::Number(p as f64)).collect()));set(&mut tree,"warnings",V::Array(warnings));Ok(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_matching_page_and_marks_missing_footnotes() {
        let run=obj(vec![("kind",string("footnoteRef")),("id",string("*"))]);
        let payload=obj(vec![
            ("version",V::Number(1.0)),("blocks",V::Array(vec![obj(vec![("kind",string("paragraph")),("runs",V::Array(vec![run]))])])),
            ("footnotes",V::Array(vec![])),("droppedRepetitions",V::Number(0.0)),("reasked",V::Bool(false))
        ]);
        let entry=obj(vec![("pageFingerprint",string("fp")),("payload",payload)]);
        let tree=obj(vec![
            ("variant",string("original")),("pages",V::Array(vec![obj(vec![("page",V::Number(1.0)),("fingerprint",string("fp")),("status",string("text"))])])),
            ("blocks",V::Array(vec![obj(vec![("id",string("old")),("page",V::Number(1.0)),("kind",string("paragraph")),("source",string("pdf"))])])),
            ("footnotes",V::Array(vec![])),("footnoteLinkFailedPages",V::Array(vec![])),("warnings",V::Array(vec![]))
        ]);
        let result=merge(tree,&V::Array(vec![entry])).unwrap();
        assert_eq!(s(&a(&result,"pages")[0],"status"),"ai");
        assert_eq!(s(&a(&result,"blocks")[1],"id"),"p1.ai1");
        assert_eq!(a(&result,"footnoteLinkFailedPages"),&[V::Number(1.0)]);
        assert_eq!(s(&a(&a(&result,"blocks")[1],"runs")[0],"id"),"p1.fn-missing-*");
    }
}
