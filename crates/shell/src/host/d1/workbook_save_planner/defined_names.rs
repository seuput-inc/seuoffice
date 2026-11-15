//! Declarative workbook defined names, retaining unmodelled entries.
use std::collections::HashSet;
use jscompat::TryJsRegex;
use serde_json::Value;
use super::{formulas, xml};
fn valid(name: &str) -> Result<bool,String> {
    if name.is_empty() || name.encode_utf16().count()>255 || matches!(name.to_lowercase().as_str(),"true"|"false") { return Ok(false); }
    let names=TryJsRegex::new(r"^[\p{L}_\\][\p{L}\p{N}_.\\]*$","u").map_err(|e|e.to_string())?;
    let cells=TryJsRegex::new(r"^(?:[A-Za-z]{1,3}[0-9]+|[Rr][0-9]*[Cc][0-9]*)$","").map_err(|e|e.to_string())?;
    Ok(names.try_test(name).map_err(|e|e.to_string())? && !cells.try_test(name).map_err(|e|e.to_string())?)
}
fn escape(s:&str)->String { s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;") }
fn prefix_additions(entries:&str,prefix:&str)->String { if prefix.is_empty() { entries.into() } else { entries.replace("<definedName",&format!("<{prefix}:definedName")).replace("</definedName",&format!("</{prefix}:definedName")) } }
fn append(xml:&str, additions:&str)->Result<String,String> {
    if let Some(section)=xml::root_elements(xml,"definedNames")?.into_iter().next() {
        let prefix=section.name.split_once(':').map_or("",|(p,_)|p);
        let entries=prefix_additions(additions,prefix);
        if section.close_start==section.end {
            if additions.is_empty() { return Ok(xml.into()); }
            let open=xml[section.start..section.open_end].trim_end_matches('>').trim_end_matches('/').trim_end();
            return Ok(format!("{}{open}>{entries}</{}>{}",&xml[..section.start],section.name,&xml[section.end..]));
        }
        let inner=&xml[section.open_end..section.close_start];
        if inner.is_empty() && entries.is_empty() { return Ok(format!("{}{}",&xml[..section.start],&xml[section.end..])); }
        return Ok(format!("{}{}{}{}",&xml[..section.open_end],inner,entries,&xml[section.close_start..]));
    }
    if additions.is_empty() { return Ok(xml.into()); }
    let anchor=xml::root_elements(xml,"sheets")?.into_iter().next().ok_or("workbook.xml has no sheets element.")?;
    let groups=xml::root_elements(xml,"functionGroups")?.into_iter().next();
    let externals=xml::root_elements(xml,"externalReferences")?.into_iter().next();
    let at=externals.map(|e|e.end).or_else(||groups.map(|e|e.end)).unwrap_or(anchor.end);
    let prefix=anchor.name.split_once(':').map_or("",|(p,_)|p);
    let name=if prefix.is_empty() { "definedNames".to_owned() } else { format!("{prefix}:definedNames") };
    Ok(format!("{}<{name}>{}</{name}>{}",&xml[..at],prefix_additions(additions,prefix),&xml[at..]))
}
pub(super) fn apply(workbook:&str,state:&Value)->Result<String,String> {
    let preserve:HashSet<&str>=state.get("preserveNames").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let names=state.get("names").and_then(Value::as_array).ok_or("Defined names must be an array.")?;
    let existing=xml::root_elements(workbook,"definedName")?;
    let mut preserved=HashSet::new();
    for element in &existing { if let Some(name)=element.attribute("name").filter(|n|preserve.contains(n)) { preserved.insert((name.to_owned(),element.attribute("localSheetId").unwrap_or("-1").to_owned())); } }
    let mut seen=HashSet::new(); let mut additions=String::new();
    for entry in names {
        let name=entry.get("name").and_then(Value::as_str).unwrap_or("");
        if !valid(name)? { return Err(format!("\"{name}\" is not a valid defined name.")); }
        if name.starts_with("_xlnm") { return Err("Names starting with \"_xlnm\" are reserved by Excel.".into()); }
        let index=entry.get("sheetIndex").and_then(Value::as_u64);
        let key=(name.to_owned(),index.map(|i|i.to_string()).unwrap_or_else(||"-1".into()));
        if preserved.contains(&key) { return Err(format!("The name \"{name}\" also exists in a form the editor cannot model — saving would duplicate it.")); }
        if !seen.insert(key) { return Err(format!("The name \"{name}\" is defined twice.")); }
        let formula=entry.get("formula").and_then(Value::as_str).unwrap_or("");
        additions.push_str(&format!("<definedName name=\"{}\"{}>{}</definedName>",escape(name).replace('"',"&quot;").replace('\'',"&apos;"),index.map(|i|format!(" localSheetId=\"{i}\"")).unwrap_or_default(),escape(&formulas::with_future_function_markers(formula.strip_prefix('=').unwrap_or(formula)))));
    }
    let mut out=workbook.to_owned();
    for e in existing.into_iter().rev() {
        let name=e.attribute("name").unwrap_or("");
        let hidden=e.attribute("hidden");
        if !name.starts_with("_xlnm") && !matches!(hidden,Some("1"|"true")) && !preserve.contains(name) { out.replace_range(e.start..e.end,""); }
    }
    append(&out,&additions)
}
#[cfg(test)] mod tests { use super::*; #[test] fn keeps_builtins_and_refuses_duplicate() {
    let xml="<workbook><sheets/><definedNames><definedName name=\"_xlnm.Print_Area\">a</definedName><definedName name=\"old\">b</definedName></definedNames></workbook>";
    let result=apply(xml,&serde_json::json!({"names":[{"name":"new_name","formula":"=1"}],"preserveNames":[]})).unwrap();
    assert!(result.contains("_xlnm.Print_Area") && result.contains("new_name") && !result.contains("name=\"old\""));
    assert!(apply(xml,&serde_json::json!({"names":[{"name":"x","formula":"1"},{"name":"x","formula":"2"}],"preserveNames":[]})).is_err());
} }
