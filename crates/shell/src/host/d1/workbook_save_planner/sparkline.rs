//! Append x14 sparkline groups to worksheet extensions.
use std::collections::{HashMap,HashSet};
use jscompat::TryJsRegex;
use serde_json::Value;
use super::{package::PackageEditor,state::PlannerState};
fn escape(s:&str)->String { s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;") }
fn splice(s:&str,at:usize,value:&str)->String { format!("{}{}{}",&s[..at],value,&s[at..]) }
fn valid_cell(cell:&str)->Result<(),String> {
    let re=TryJsRegex::new(r"^\$?([A-Z]+)\$?([1-9][0-9]*)$","").map_err(|e|e.to_string())?;
    let found=re.try_exec(cell.trim_matches(jscompat::is_js_space)).map_err(|e|e.to_string())?.ok_or_else(||format!("Invalid cell address: {cell}"))?;
    let mut column=0u64;
    for ch in found.get(1).unwrap_or("").bytes() { column=column.saturating_mul(26).saturating_add((ch-b'A'+1) as u64); }
    let row=found.get(2).unwrap_or("").parse::<u64>().unwrap_or(u64::MAX);
    if row>1_048_576 || column>16_384 { return Err(format!("\"{cell}\" is outside the worksheet grid.")); }
    Ok(())
}
fn group(item:&Value)->Result<String,String> {
    let kind=item.get("type").and_then(Value::as_str).unwrap_or("");
    let attr=if kind=="line" { String::new() } else { format!(" type=\"{kind}\"") };
    let color=if let Some(c)=item.get("color").and_then(Value::as_str) {
        let hex=c.trim_matches(jscompat::is_js_space);
        if hex.len()!=7 || !hex.starts_with('#') || !hex[1..].bytes().all(|b|b.is_ascii_hexdigit()) { return Err(format!("\"{c}\" is not a #RRGGBB color.")); }
        format!("FF{}",hex[1..].to_ascii_uppercase())
    } else { "FF376092".into() };
    let mut body=String::new();
    for cell in item.get("cells").and_then(Value::as_array).ok_or("Sparkline group has no cells.")? {
        let host=cell.get("cell").and_then(Value::as_str).ok_or("Sparkline cell has no address.")?;
        let source=cell.get("sourceRef").and_then(Value::as_str).ok_or("Sparkline cell has no source.")?;
        valid_cell(host)?;
        body.push_str(&format!("<x14:sparkline><xm:f>{}</xm:f><xm:sqref>{host}</xm:sqref></x14:sparkline>",escape(source)));
    }
    Ok(format!("<x14:sparklineGroup displayEmptyCellsAs=\"gap\"{attr}><x14:colorSeries rgb=\"{color}\"/><x14:colorNegative rgb=\"FFD00000\"/><x14:sparklines>{body}</x14:sparklines></x14:sparklineGroup>"))
}
fn add(xml:&str,groups:&[&Value])->Result<String,String> {
    let mut seen=HashSet::new();
    for item in groups { for cell in item.get("cells").and_then(Value::as_array).ok_or("Sparkline group has no cells.")? {
        let cell=cell.get("cell").and_then(Value::as_str).ok_or("Sparkline cell has no address.")?;
        if !seen.insert(cell) || xml.contains(&format!("<xm:sqref>{cell}</xm:sqref>")) { return Err(format!("{cell} already has a sparkline.")); }
    }}
    let mut body=String::new(); for item in groups { body.push_str(&group(item)?); }
    let wrap=|body:&str| format!("<x14:sparklineGroups xmlns:xm=\"http://schemas.microsoft.com/office/excel/2006/main\">{body}</x14:sparklineGroups>");
    let ext=TryJsRegex::new(r#"<ext\b[^>]*\buri="\{05C60535-1F16-4FD2-B633-F4F36F0B64E0\}"[^>]*>"#,"i").map_err(|e|e.to_string())?;
    if let Some(found)=ext.try_exec(xml).map_err(|e|e.to_string())?.filter(|m|!m.as_str().ends_with("/>")) {
        let close=xml[found.start()..].find("</ext>").map(|at|found.start()+at).ok_or("The sparkline ext element is malformed.")?;
        let block=&xml[found.start()..close];
        return Ok(if let Some(at)=block.rfind("</x14:sparklineGroups>") { splice(xml,found.start()+at,&body) } else { splice(xml,close,&wrap(&body)) });
    }
    let extension=format!("<ext uri=\"{{05C60535-1F16-4fd2-B633-F4F36F0B64E0}}\" xmlns:x14=\"http://schemas.microsoft.com/office/spreadsheetml/2009/9/main\">{}</ext>",wrap(&body));
    if let Some(at)=xml.rfind("</extLst>") { if xml.rfind("</sheetData>").map_or(true,|end|at>end) { return Ok(splice(xml,at,&extension)); } }
    let at=xml.rfind("</worksheet>").ok_or("Worksheet has no closing element.")?;
    Ok(splice(xml,at,&format!("<extLst>{extension}</extLst>")))
}
pub(super) fn apply(pkg:&mut PackageEditor<'_>,state:&mut PlannerState,request:&Value,touched:&mut HashSet<String>)->Result<(),String> {
    let mut groups:Vec<(&str,Vec<&Value>)>=Vec::new(); let mut indices=HashMap::new();
    for item in request.get("sparklineAdditions").and_then(Value::as_array).into_iter().flatten() {
        let name=item.get("sheetName").and_then(Value::as_str).ok_or("Sparkline group has no sheet name.")?;
        let index=*indices.entry(name).or_insert_with(||{ groups.push((name,Vec::new())); groups.len()-1 });
        groups[index].1.push(item);
    }
    for (name,groups) in groups { let path=state.path_for(pkg,name)?; let xml=pkg.read_text(&path)?; pkg.write(&path,add(&xml,&groups)?)?; touched.insert(path); }
    Ok(())
}
#[cfg(test)] mod tests { use super::*; #[test] fn adds_and_rejects_duplicate_hosts() {
    let group=serde_json::json!({"type":"line","cells":[{"cell":"A1","sourceRef":"Sheet1!B1:B2"}]});
    let xml=add("<worksheet><sheetData/></worksheet>",&[&group]).unwrap();
    assert!(xml.contains("<xm:sqref>A1</xm:sqref>"));
    assert!(add(&xml,&[&group]).is_err());
    assert!(valid_cell("XFE1").is_err());
} }
