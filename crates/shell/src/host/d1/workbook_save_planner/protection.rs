//! Password-preserving worksheet and workbook protection writers.
use jscompat::TryJsRegex;
use serde_json::Value;
use std::sync::Arc;
use super::state::PlannerState;

const SHEET: &str = r"<sheetProtection\b[^>]*\/>|<sheetProtection\b[^>]*>\s*<\/sheetProtection>";
const WORKBOOK: &str = r"<workbookProtection\b[^>]*\/>|<workbookProtection\b[^>]*>\s*<\/workbookProtection>";
const RANGES: &str = r"<protectedRanges\b[^>]*\/>|<protectedRanges\b[^>]*>[\s\S]*?<\/protectedRanges>";
fn find(text: &str, pattern: &str) -> Result<Option<(usize, usize)>, String> {
    let re = TryJsRegex::new(pattern, "").map_err(|e| e.to_string())?;
    Ok(re.try_exec(text).map_err(|e| e.to_string())?.map(|m| (m.start(), m.end())))
}
fn has(text: &str, pattern: &str) -> Result<bool, String> { Ok(find(text, pattern)?.is_some()) }
fn splice(text: &str, start: usize, end: usize, insert: &str) -> String {
    let mut out = String::with_capacity(text.len() + insert.len());
    out.push_str(&text[..start]); out.push_str(insert); out.push_str(&text[end..]); out
}
fn escape(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }
fn string<'a>(value: &'a Value, key: &str) -> &'a str { value.get(key).and_then(Value::as_str).unwrap_or("") }
fn password_attributes(value: Option<&Value>) -> String {
    let Some(p) = value.filter(|p| !p.is_null()) else { return String::new(); };
    if p.get("legacy").is_some() { return format!(" password=\"{}\"", string(p, "legacy")); }
    format!(" algorithmName=\"{}\" hashValue=\"{}\" saltValue=\"{}\" spinCount=\"{}\"", escape(string(p,"algorithmName")), escape(string(p,"hashValue")), escape(string(p,"saltValue")), p.get("spinCount").map(Value::to_string).unwrap_or_default())
}
fn protection_element(write: &Value) -> String {
    let mut out = format!("<sheetProtection{} sheet=\"1\"", password_attributes(write.get("password")));
    let allow = write.get("allow");
    for name in ["objects", "scenarios", "formatCells", "formatColumns", "formatRows", "insertColumns", "insertRows", "insertHyperlinks", "deleteColumns", "deleteRows", "selectLockedCells", "sort", "autoFilter", "pivotTables", "selectUnlockedCells"] {
        let default_allowed = matches!(name, "objects" | "scenarios" | "selectLockedCells" | "selectUnlockedCells");
        let enabled = allow.and_then(|v| v.get(name)).and_then(Value::as_bool).unwrap_or(matches!(name, "selectLockedCells" | "selectUnlockedCells"));
        if default_allowed && !enabled { out.push_str(&format!(" {name}=\"1\"")); }
        else if !default_allowed && enabled { out.push_str(&format!(" {name}=\"0\"")); }
    }
    out.push_str("/>"); out
}
fn apply_sheet(xml: &str, write: &Value) -> Result<String, String> {
    let protected = write.as_bool().unwrap_or_else(|| write.get("protected").and_then(Value::as_bool).unwrap_or(false));
    let existing = find(xml, SHEET)?;
    if !protected {
        let Some((a,b)) = existing else { return Ok(xml.into()); };
        if !write.get("verified").and_then(Value::as_bool).unwrap_or(false) && has(&xml[a..b], r#"\b(?:password|hashValue)=""#)? { return Err("This sheet is protected with a password — removing its protection is not supported.".into()); }
        return Ok(splice(xml,a,b,""));
    }
    let explicit = write.get("allow").is_some() || write.get("password").is_some();
    let element = explicit.then(|| protection_element(write));
    if let Some((a,b)) = existing {
        let old = &xml[a..b];
        if let Some(new) = element { return Ok(splice(xml,a,b,&new)); }
        if has(old, r#"\bsheet="(?:1|true)""#)? { return Ok(xml.into()); }
        let next = if let Some((x,y)) = find(old, r#" sheet="[^"]*""#)? { splice(old,x,y," sheet=\"1\"") } else { old.replacen("<sheetProtection", "<sheetProtection sheet=\"1\"", 1) };
        return Ok(splice(xml,a,b,&next));
    }
    let anchor = match find(xml, r"<sheetCalcPr\b[^>]*\/?>")? {
        Some(anchor) => Some(anchor),
        None => find(xml, r"<\/sheetData>|<sheetData\b[^>]*\/>")?,
    }.ok_or("Worksheet has no sheetData element.")?;
    Ok(splice(xml,anchor.1,anchor.1,element.as_deref().unwrap_or("<sheetProtection sheet=\"1\" objects=\"1\" scenarios=\"1\"/>")))
}
fn apply_ranges(xml: &str, ranges: &[Value]) -> Result<String, String> {
    let existing = find(xml, RANGES)?;
    if let Some((a,b)) = existing {
        if has(&xml[a..b], r#"\b(?:password|hashValue)="|securityDescriptor"#)? { return Err("This sheet has password- or permission-protected edit ranges — editing them is not supported.".into()); }
    }
    let stripped = existing.map_or_else(|| xml.to_owned(), |(a,b)| splice(xml,a,b,""));
    if ranges.is_empty() { return Ok(stripped); }
    let mut element = String::from("<protectedRanges>");
    for range in ranges { element.push_str(&format!("<protectedRange sqref=\"{}\" name=\"{}\"{}/>", escape(string(range,"sqref")), escape(string(range,"name")), password_attributes(range.get("password")))); }
    element.push_str("</protectedRanges>");
    let anchor = match find(&stripped,r"<sheetProtection\b[^>]*\/?>")? {
        Some(anchor) => Some(anchor),
        None => match find(&stripped,r"<sheetCalcPr\b[^>]*\/?>")? {
            Some(anchor) => Some(anchor),
            None => find(&stripped,r"<\/sheetData>|<sheetData\b[^>]*\/>")?,
        },
    }.ok_or("Worksheet has no sheetData element.")?;
    Ok(splice(&stripped,anchor.1,anchor.1,&element))
}
pub(super) fn sheets(state: &mut PlannerState, request: &Value) -> Result<(), String> {
    for item in request.get("sheetProtections").and_then(Value::as_array).into_iter().flatten() {
        if let Some(sheet) = state.worksheet_mut(string(item,"sheetName")) { sheet.xml = Arc::new(apply_sheet(&sheet.xml,item)?); }
    }
    Ok(())
}
pub(super) fn ranges(state: &mut PlannerState, request: &Value) -> Result<(), String> {
    for item in request.get("protectedRangeStates").and_then(Value::as_array).into_iter().flatten() {
        if let Some(sheet) = state.worksheet_mut(string(item,"sheetName")) { sheet.xml = Arc::new(apply_ranges(&sheet.xml,item.get("ranges").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]))?); }
    }
    Ok(())
}
pub(super) fn workbook(xml: &str, lock: bool) -> Result<String, String> {
    let existing = find(xml, WORKBOOK)?;
    if !lock {
        let Some((a,b)) = existing else { return Ok(xml.into()); };
        let old = &xml[a..b];
        if has(old,r#"\bworkbook(?:Password|HashValue)=""#)? { return Err("The workbook structure is protected with a password — removing its protection is not supported.".into()); }
        let stripped = if let Some((x,y)) = find(old,r#"\s+lockStructure="[^"]*""#)? { splice(old,x,y,"") } else { old.to_owned() };
        let empty = has(&stripped,r"^<workbookProtection\s*(?:\/>|>\s*<\/workbookProtection>)$")?;
        return Ok(splice(xml,a,b,if empty { "" } else { &stripped }));
    }
    if let Some((a,b)) = existing {
        let old = &xml[a..b];
        if has(old,r#"\blockStructure="(?:1|true)""#)? { return Ok(xml.into()); }
        let next = if let Some((x,y)) = find(old,r#"\s+lockStructure="[^"]*""#)? { splice(old,x,y," lockStructure=\"1\"") } else { old.replacen("<workbookProtection","<workbookProtection lockStructure=\"1\"",1) };
        return Ok(splice(xml,a,b,&next));
    }
    let anchor = find(xml,r"<bookViews\b|<sheets\b")?.ok_or("Workbook has no sheets element.")?;
    Ok(splice(xml,anchor.0,anchor.0,"<workbookProtection lockStructure=\"1\"/>"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn protected_removal_fails_closed() {
        assert!(apply_sheet("<worksheet><sheetData/><sheetProtection password=\"ABCD\"/></worksheet>",&serde_json::json!({"protected":false})).is_err());
        assert!(apply_ranges("<worksheet><sheetData/><protectedRanges><protectedRange securityDescriptor=\"x\"/></protectedRanges></worksheet>",&[]).is_err());
        assert!(workbook("<workbook><workbookProtection workbookPassword=\"ABCD\"/><sheets/></workbook>",false).is_err());
    }
    #[test] fn protection_is_inserted_after_data() {
        assert_eq!(apply_sheet("<worksheet><sheetData/></worksheet>",&serde_json::json!({"protected":true})).unwrap(),"<worksheet><sheetData/><sheetProtection sheet=\"1\" objects=\"1\" scenarios=\"1\"/></worksheet>");
    }
    #[test] fn verified_unlock_and_permission_preservation() {
        let xml = "<worksheet><sheetData/><sheetProtection password=\"ABCD\" formatCells=\"0\"/></worksheet>";
        assert_eq!(apply_sheet(xml, &serde_json::json!({"protected":false,"verified":true})).unwrap(), "<worksheet><sheetData/></worksheet>");
        let unchanged = apply_sheet(xml, &serde_json::json!({"protected":true})).unwrap();
        assert!(unchanged.contains("password=\"ABCD\"") && unchanged.contains("formatCells=\"0\"") && unchanged.contains("sheet=\"1\""));
        let workbook_xml = "<workbook><workbookProtection lockWindows=\"1\" lockStructure=\"1\"/><sheets/></workbook>";
        assert_eq!(workbook(workbook_xml, false).unwrap(), "<workbook><workbookProtection lockWindows=\"1\"/><sheets/></workbook>");
    }
}
