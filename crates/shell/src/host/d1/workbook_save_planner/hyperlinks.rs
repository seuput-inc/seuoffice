//! Surgical worksheet hyperlink and relationship edits.
use std::{collections::HashSet, sync::Arc};
use jscompat::TryJsRegex;
use serde_json::Value;
use super::{cells, package::PackageEditor, sheets, state::PlannerState};
const EMPTY: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"></Relationships>";
fn regex(s: &str) -> Result<TryJsRegex,String> { TryJsRegex::new(s, "").map_err(|e| e.to_string()) }
fn splice(s: &str, a: usize, b: usize, value: &str) -> String { format!("{}{}{}", &s[..a], value, &s[b..]) }
fn find(s: &str, pattern: &str) -> Result<Option<(usize,usize)>,String> { Ok(regex(pattern)?.try_exec(s).map_err(|e| e.to_string())?.map(|m| (m.start(),m.end()))) }
fn escape(s: &str) -> String { s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;") }
fn namespace(xml: &str) -> Result<String,String> {
    let root = regex(r#"<(?![?!])([^\s/>]+)(?:[^>"']|"[^"]*"|'[^']*')*>"#)?;
    let Some(found) = root.try_exec(xml).map_err(|e|e.to_string())? else { return Ok(xml.into()); };
    if regex(r"\bxmlns:r\s*=")?.try_test(found.as_str()).map_err(|e|e.to_string())? { return Ok(xml.into()); }
    let at = found.start()+1+found.get(1).unwrap_or("").len();
    Ok(splice(xml,at,at," xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\""))
}
fn apply_edits(xml: &str, rels: Option<&str>, edits: &[Value]) -> Result<(String,Option<String>,bool),String> {
    let mut xml = xml.to_owned(); let mut rels = rels.map(str::to_owned); let mut changed = false;
    for edit in edits {
        let address = cells::address(cells::coordinate(edit,"row")?+1,cells::coordinate(edit,"column")?);
        let pattern = format!(r#"<hyperlink\b[^>]*?\bref="{address}"[^>]*/>"#);
        if let Some((a,b)) = find(&xml,&pattern)? {
            let old = xml[a..b].to_owned();
            let id_re = regex(r#"\br:id="([^"]+)""#)?;
            let id = id_re.try_exec(&old).map_err(|e|e.to_string())?.and_then(|m| m.get(1).map(str::to_owned));
            xml = splice(&xml,a,b,"");
            if let (Some(id),Some(current)) = (id,rels.as_mut()) {
                let refs = regex(r#"<hyperlink\b[^>]*?\br:id="([^"]+)""#)?;
                let mut used = false;
                for m in refs.try_find_iter(&xml) { if m.map_err(|e|e.to_string())?.get(1)==Some(id.as_str()) { used=true; break; } }
                if !used {
                    let rel_re = regex(r#"<Relationship\b[^>]*/>"#)?;
                    let attr = regex(r#"\bId=["']([^"']*)["']"#)?;
                    let mut pos = None;
                    for m in rel_re.try_find_iter(current) { let m=m.map_err(|e|e.to_string())?; if attr.try_exec(m.as_str()).map_err(|e|e.to_string())?.and_then(|v|v.get(1).map(str::to_owned)).as_deref()==Some(id.as_str()) { pos=Some((m.start(),m.end())); break; } }
                    if let Some((a,b))=pos { *current=splice(current,a,b,""); changed=true; }
                }
            }
        }
        let Some(target) = edit.get("target").and_then(Value::as_str) else { continue; };
        let element = if let Some(location)=target.strip_prefix('#') { format!("<hyperlink ref=\"{address}\" location=\"{}\"/>",escape(location)) } else {
            let current = rels.get_or_insert_with(|| EMPTY.into());
            let id = sheets::next_free_relationship_id(current);
            let rel = format!("<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" Target=\"{}\" TargetMode=\"External\"/>",escape(target));
            if current.contains("</Relationships>") { *current=current.replacen("</Relationships>",&format!("{rel}</Relationships>"),1); }
            else { let re=regex(r"<Relationships\b([^>]*)/>")?; if let Some(m)=re.try_exec(current).map_err(|e|e.to_string())? { let replacement=format!("<Relationships{}>{rel}</Relationships>",m.get(1).unwrap_or("")); *current=splice(current,m.start(),m.end(),&replacement); } }
            changed=true; format!("<hyperlink ref=\"{address}\" r:id=\"{id}\"/>")
        };
        if xml.contains("</hyperlinks>") { xml=xml.replacen("</hyperlinks>",&format!("{element}</hyperlinks>"),1); }
        else {
            let section=format!("<hyperlinks>{element}</hyperlinks>");
            let anchor=find(&xml,r"<printOptions\b|<pageMargins\b|<pageSetup\b|<headerFooter\b|<rowBreaks\b|<colBreaks\b|<drawing\b|<legacyDrawing\b|<picture\b|<oleObjects\b|<tableParts\b|<extLst\b")?.map(|p|p.0).or_else(||xml.rfind("</worksheet>")).ok_or("Worksheet has no closing element.")?;
            xml=splice(&xml,anchor,anchor,&section);
        }
    }
    if let Some((a,b))=find(&xml,r"<hyperlinks>\s*<\/hyperlinks>")? { xml=splice(&xml,a,b,""); }
    Ok((namespace(&xml)?,rels,changed))
}
pub(super) fn apply(pkg:&mut PackageEditor<'_>, state:&mut PlannerState, request:&Value, touched:&mut HashSet<String>) -> Result<(),String> {
    for item in request.get("hyperlinkEdits").and_then(Value::as_array).into_iter().flatten() {
        let Some(edits)=item.get("edits").and_then(Value::as_array).filter(|v|!v.is_empty()) else { continue; };
        let Some(name)=item.get("sheetName").and_then(Value::as_str) else { continue; };
        let Some(sheet)=state.worksheet_mut(name) else { continue; };
        let path=sheet.path.clone();
        let Some(file)=path.strip_prefix("xl/worksheets/").filter(|p| !p.contains('/')) else { continue; };
        let rel_path=format!("xl/worksheets/_rels/{file}.rels");
        let exists=pkg.has(&rel_path)?;
        let rels=if exists { Some(pkg.read_text(&rel_path)?) } else { None };
        let (xml,next,changed)=apply_edits(&sheet.xml,rels.as_deref().map(String::as_str),edits)?;
        sheet.xml=Arc::new(xml);
        if changed { if let Some(next)=next { if exists { pkg.write(&rel_path,next)?; } else { pkg.add(&rel_path,next); } touched.insert(rel_path); } }
    }
    Ok(())
}
#[cfg(test)] mod tests { use super::*; #[test] fn external_link_is_removed_and_internal_inserted() {
    let xml="<worksheet><sheetData/><hyperlinks><hyperlink ref=\"A1\" r:id=\"rId1\"/></hyperlinks></worksheet>";
    let rel="<Relationships><Relationship Id=\"rId1\" Target=\"x\"/></Relationships>";
    let (xml,rels,changed)=apply_edits(xml,Some(rel),&[serde_json::json!({"row":0,"column":0,"target":"#S!B2"})]).unwrap();
    assert!(changed && xml.contains("location=\"S!B2\"") && !rels.unwrap().contains("rId1"));
} }
