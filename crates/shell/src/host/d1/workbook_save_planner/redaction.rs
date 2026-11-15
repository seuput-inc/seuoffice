//! The redaction record follows sheet copies, renames and structural moves.
use std::collections::{HashMap, HashSet};
use super::{package::PackageEditor, xml};

const PART: &str = "xl/gxRedactions.json";
const RELS: &str = "xl/_rels/workbook.xml.rels";
const TYPES: &str = "[Content_Types].xml";
const REL_TYPE: &str = "https://schemas.genspark.ai/genoffice/2026/relationships/redactions";
use serde_json::{Value, json};

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> { value.get(key).and_then(Value::as_str) }
fn list<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match value.get(key) { None | Some(Value::Null) => Ok(&[]), Some(Value::Array(items)) => Ok(items), _ => Err(format!("{key} must be an array")) }
}
fn integer(value: &Value, key: &str) -> Result<i64, String> {
    value.get(key).and_then(Value::as_i64).ok_or_else(|| format!("Invalid redaction coordinate {key}."))
}
fn moved_range(start: i64, end: i64, op: &Value) -> Result<Option<(i64, i64)>, String> {
    let index = integer(op, "index")?;
    let count = integer(op, "count")?;
    if index < 0 || count <= 0 { return Err("Invalid structural operation.".into()); }
    let last = index.checked_add(count - 1).ok_or("Structural operation overflow.")?;
    let kind = text(op, "kind").ok_or("Invalid structural operation.")?;
    if kind == "move-rows" {
        let before = integer(op, "before")?;
        let after = index.checked_add(count).ok_or("Structural operation overflow.")?;
        if before >= index && before <= after { return Err("A row move has a target inside the moved block — aborted.".into()); }
        let (a, b, c, d) = if before > index { (index, last, after, before - 1) } else { (before, index - 1, index, last) };
        if end < a || start > d || (start <= a && end >= d) { return Ok(Some((start, end))); }
        if start >= a && end <= b { let delta = d - c + 1; return Ok(Some((start + delta, end + delta))); }
        if start >= c && end <= d { let delta = b - a + 1; return Ok(Some((start - delta, end - delta))); }
        return Err("A range partially overlaps the moved rows — the move cannot be saved.".into());
    }
    if kind == "insert-rows" || kind == "insert-cols" {
        let shift = |n: i64| if n >= index { n.checked_add(count).ok_or("Structural operation overflow.") } else { Ok(n) };
        return Ok(Some((shift(start)?, shift(end)?)));
    }
    if kind == "remove-rows" || kind == "remove-cols" {
        if start >= index && end <= last { return Ok(None); }
        let first = if start > last { start - count } else if start >= index { index } else { start };
        let final_ = if end > last { end - count } else if end >= index { index - 1 } else { end };
        return Ok((final_ >= first).then_some((first, final_)));
    }
    Err(format!("Unknown structural operation {kind}."))
}
fn moved_mark(mark: &Value, ops: &[Value]) -> Result<Option<Value>, String> {
    let mut value = mark.clone();
    for op in ops.iter().filter(|op| op.get("index").is_some()) {
        let kind = text(op, "kind").ok_or("Invalid structural operation.")?;
        let (from, to) = if kind == "insert-cols" || kind == "remove-cols" { ("startColumn", "endColumn") } else { ("startRow", "endRow") };
        let Some((start, end)) = moved_range(integer(&value, from)?, integer(&value, to)?, op)? else { return Ok(None); };
        let object = value.as_object_mut().ok_or("Invalid redaction mark.")?;
        object.insert(from.into(), json!(start)); object.insert(to.into(), json!(end));
    }
    Ok(Some(value))
}
fn marks_for<'a>(state: &'a Value) -> Result<&'a [Value], String> { list(state, "marks") }
fn moved_marks(marks: &[Value], sheet: &str, structural: &[Value]) -> Result<Vec<Value>, String> {
    let ops = match structural.iter().find(|op| text(op, "sheetName") == Some(sheet)) {
        Some(entry) => list(entry, "ops")?, None => &[],
    };
    let mut result = Vec::new();
    for mark in marks { if let Some(mark) = moved_mark(mark, ops)? { result.push(mark); } }
    Ok(result)
}
/// Input states are validated by the original redaction record reader.
pub(super) fn rekey(states: &[Value], plan: Option<&Value>, structural: &[Value]) -> Result<Vec<Value>, String> {
    let renames = match plan { Some(plan) => list(plan, "renames")?, None => &[] };
    let removals = match plan { Some(plan) => list(plan, "removals")?, None => &[] };
    let additions = match plan { Some(plan) => list(plan, "additions")?, None => &[] };
    let renamed: HashMap<&str, &str> = renames.iter().filter_map(|v| Some((text(v, "sheetName")?, text(v, "newName")?))).collect();
    let removed: HashSet<&str> = removals.iter().filter_map(Value::as_str).collect();
    let added: HashSet<&str> = additions.iter().filter_map(|v| text(v, "name")).collect();
    let is_added = |state: &Value| text(state, "sheetName").is_some_and(|name| added.contains(name) && !removed.contains(name) && !renamed.contains_key(name));
    let mut result = Vec::new();
    for state in states.iter().filter(|state| !is_added(state)) {
        let name = text(state, "sheetName").ok_or("Invalid redaction sheet.")?;
        if !removed.contains(name) {
            result.push(json!({"sheetName":renamed.get(name).copied().unwrap_or(name),"marks":moved_marks(marks_for(state)?, name, structural)?}));
        }
    }
    for addition in additions {
        let name = text(addition, "name").ok_or("Invalid added sheet.")?;
        let source = text(addition, "sourceSheetName");
        let mut seen = HashSet::new();
        let mut marks = Vec::new();
        for state in states.iter().filter(|state| (!is_added(state) && source.is_some() && text(state, "sheetName") == source) || (is_added(state) && text(state, "sheetName") == Some(name))) {
            for mark in marks_for(state)? {
                let key = serde_json::to_string(&json!([mark.get("startRow"), mark.get("endRow"), mark.get("startColumn"), mark.get("endColumn"), mark.get("label")])).map_err(|e| e.to_string())?;
                if seen.insert(key) { marks.push(mark.clone()); }
            }
        }
        if !marks.is_empty() { result.push(json!({"sheetName":name,"marks":moved_marks(&marks, name, structural)?})); }
    }
    Ok(result)
}

/// Prepare the withheld-cell record before cell mutation, as in gateway stage 1457.
pub(super) fn prepare(
    pkg: &mut PackageEditor<'_>,
    touched: &mut HashSet<String>,
    request: &Value,
    original_workbook: &str,
) -> Result<Option<Vec<Value>>, String> {
    let structural = list(request, "structuralOps")?;
    let plan = request.get("sheetPlan").filter(|v| !v.is_null());
    let mut moves = false;
    for sheet in structural {
        if list(sheet, "ops")?.iter().any(|op| op.get("index").is_some()) { moves = true; break; }
    }
    if let Some(plan) = plan {
        let names: Vec<_> = xml::root_elements(original_workbook, "sheet")?.into_iter().filter_map(|element| element.attribute("name").map(str::to_owned)).collect();
        let order = list(plan, "order")?;
        moves |= !list(plan, "renames")?.is_empty() || !list(plan, "removals")?.is_empty() || !list(plan, "additions")?.is_empty()
            || order.len() != names.len() || order.iter().zip(&names).any(|(name, original)| name.as_str() != Some(original.as_str()));
    }
    let explicit = request.get("redactionStates").filter(|v| !v.is_null());
    let states = if let Some(value) = explicit {
        Some(value.as_array().ok_or("redactionStates must be an array")?.clone())
    } else if moves {
        recorded(pkg, original_workbook)?
    } else { None };
    let Some(states) = states else { return Ok(None); };
    let result = rekey(&states, plan, structural)?;
    apply_part(pkg, touched, &result)?;
    Ok(Some(result))
}

fn write_order(states: &[Value]) -> Result<Vec<&Value>, String> {
    let mut result = Vec::new();
    for state in states {
        if !marks_for(state)?.is_empty() {
            text(state, "sheetName").ok_or("Invalid redaction sheet.")?;
            result.push(state);
        }
    }
    result.sort_by(|left, right| text(left, "sheetName").unwrap_or("").encode_utf16().cmp(text(right, "sheetName").unwrap_or("").encode_utf16()));
    Ok(result)
}

fn serialize(states: &[Value]) -> Result<String, String> {
    let mut sheets = Vec::new();
    for state in write_order(states)? {
        let mut marks = marks_for(state)?.to_vec();
        marks.sort_by_key(|mark| ["startRow", "startColumn", "endRow", "endColumn"].map(|key| mark.get(key).and_then(Value::as_i64).unwrap_or(0)));
        let mut output = Vec::new();
        for mark in marks {
            let mut object = serde_json::Map::new();
            for key in ["startRow", "endRow", "startColumn", "endColumn", "label"] {
                object.insert(key.into(), mark.get(key).cloned().ok_or("Invalid redaction mark.")?);
            }
            if let Some(fill) = mark.get("previousFill") { object.insert("previousFill".into(), fill.clone()); }
            output.push(Value::Object(object));
        }
        sheets.push(json!({"name":text(state, "sheetName").ok_or("Invalid redaction sheet.")?,"marks":output}));
    }
    serde_json::to_string_pretty(&json!({"version":1,"sheets":sheets})).map(|s| format!("{s}\n")).map_err(|e| e.to_string())
}

/// Read an existing record or its defined-name mirror; damaged records fail closed.
pub(super) fn recorded(pkg: &mut PackageEditor<'_>, workbook: &str) -> Result<Option<Vec<Value>>, String> {
    let mut damage = None;
    match pkg.has(PART) {
        Ok(true) => match pkg.read_text(PART).and_then(|text| file_parse::xlsx::workbook_redaction_part(&text)) {
            Ok(Value::Array(states)) => return Ok(states.iter().any(|s| marks_for(s).is_ok_and(|m| !m.is_empty())).then_some(states)),
            Ok(_) => damage = Some("Invalid redaction record.".to_owned()),
            Err(error) => damage = Some(error),
        },
        Ok(false) => {}
        Err(error) => damage = Some(error),
    }
    let parsed = file_parse::xlsx::workbook_redaction_mirror(workbook);
    let result = match parsed {
        Ok(Value::Array(states)) if damage.is_none() || !states.is_empty() => Ok(states),
        Ok(_) => Err(damage.unwrap_or_else(|| "Invalid redaction mirror.".into())),
        Err(error) => Err(error),
    }.map_err(|error| format!("The record of the cells hidden from AI cannot be read, so this save cannot move rows, columns or sheets: the hidden cells would no longer be covered. Undo the row, column and sheet changes and save again. ({error})"))?;
    Ok(result.iter().any(|s| marks_for(s).is_ok_and(|m| !m.is_empty())).then_some(result))
}

/// Write or remove the authoritative JSON part and its package declarations.
pub(super) fn apply_part(pkg: &mut PackageEditor<'_>, touched: &mut HashSet<String>, states: &[Value]) -> Result<(), String> {
    let empty = states.iter().try_fold(true, |empty, state| Ok::<_, String>(empty && marks_for(state)?.is_empty()))?;
    if empty {
        if !pkg.has(PART)? { return Ok(()); }
        pkg.remove(PART);
        touched.insert(PART.into());
        let rels = pkg.read_text(RELS)?;
        let matches: Vec<_> = xml::root_elements(&rels, "Relationship")?.into_iter().filter(|element| element.attribute("Type") == Some(REL_TYPE)).collect();
        let updated = xml::remove_elements(&rels, &matches);
        if updated != *rels { pkg.write(RELS, updated)?; touched.insert(RELS.into()); }
        let types = pkg.read_text(TYPES)?;
        let matches: Vec<_> = xml::root_elements(&types, "Override")?.into_iter().filter(|element| element.attribute("PartName") == Some("/xl/gxRedactions.json")).collect();
        let updated = xml::remove_elements(&types, &matches);
        if updated != *types { pkg.write(TYPES, updated)?; touched.insert(TYPES.into()); }
        return Ok(());
    }
    let content = serialize(states)?;
    if pkg.has(PART)? { pkg.write(PART, content)?; } else { pkg.add(PART, content); }
    touched.insert(PART.into());
    let rels = pkg.read_text(RELS)?;
    let relationships = xml::root_elements(&rels, "Relationship")?;
    if !relationships.iter().any(|element| element.attribute("Type") == Some(REL_TYPE)) {
        let id_re = jscompat::TryJsRegex::new(r#"\bId\s*=\s*(["'])rId(\d+)\1"#, "").map_err(|e| e.to_string())?;
        let mut used = HashSet::new();
        for found in id_re.try_find_iter(&rels) {
            let found = found.map_err(|e| e.to_string())?;
            if let Some(number) = found.get(2) { used.insert(format!("rId{number}")); }
        }
        let mut index = 1usize;
        while used.contains(format!("rId{index}").as_str()) { index += 1; }
        let updated = xml::append_to_root(&rels, "Relationships", |prefix| format!("<{prefix}Relationship Id=\"rId{index}\" Type=\"{REL_TYPE}\" Target=\"gxRedactions.json\"/>"))?;
        pkg.write(RELS, updated)?;
        touched.insert(RELS.into());
    }
    let types = pkg.read_text(TYPES)?;
    if !xml::root_elements(&types, "Override")?.iter().any(|element| element.attribute("PartName") == Some("/xl/gxRedactions.json")) {
        let updated = xml::append_to_root(&types, "Types", |prefix| format!("<{prefix}Override PartName=\"/xl/gxRedactions.json\" ContentType=\"application/json\"/>"))?;
        pkg.write(TYPES, updated)?;
        touched.insert(TYPES.into());
    }
    Ok(())
}

fn mirror_formula(sheet: usize, mark: &Value, label: &str) -> Result<String, String> {
    let payload = json!({"v":1,"sheet":sheet,"from":[integer(mark,"startRow")?,integer(mark,"startColumn")?],"to":[integer(mark,"endRow")?,integer(mark,"endColumn")?],"label":label});
    let json = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    Ok(format!("\"{}\"", json.replace('"', "\"\"")))
}

/// Replace the hidden defined-name mirror without changing unrelated workbook XML.
pub(super) fn apply_mirror(workbook: &str, states: &[Value], sheet_order: &[String]) -> Result<String, String> {
    let mirror_re = jscompat::TryJsRegex::new(r"^_gxRedaction_\d+$", "").map_err(|e| e.to_string())?;
    let mut removed = Vec::new();
    for element in xml::root_elements(workbook, "definedName")? {
        if let Some(name) = element.attribute("name") && mirror_re.try_test(name).map_err(|e| e.to_string())? { removed.push(element); }
    }
    let stripped = xml::remove_elements(workbook, &removed);
    let mut additions = Vec::new();
    for state in write_order(states)? {
        let Some(sheet) = sheet_order.iter().position(|name| Some(name.as_str()) == text(state, "sheetName")) else { continue; };
        for mark in marks_for(state)? {
            let label = text(mark, "label").ok_or("Invalid redaction label.")?;
            let overhead = mirror_formula(sheet, mark, "")?.chars().count();
            let room = 255usize.saturating_sub(overhead);
            let label = if label.chars().count() <= room {
                label.to_owned()
            } else {
                let mut shortened: String = label.chars().take(room.saturating_sub(1)).collect();
                shortened.push('…');
                shortened
            };
            let formula = mirror_formula(sheet, mark, &label)?.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            additions.push(format!("<definedName name=\"_gxRedaction_{}\" hidden=\"1\">{formula}</definedName>", additions.len() + 1));
        }
    }
    if additions.is_empty() { return Ok(stripped); }
    let mut entries = additions.join("");
    if let Some(section) = xml::root_elements(&stripped, "definedNames")?.into_iter().next() {
        let prefix = section.name.split_once(':').map_or("", |(prefix, _)| prefix);
        if !prefix.is_empty() {
            entries = entries.replace("<definedName ", &format!("<{prefix}:definedName ")).replace("</definedName>", &format!("</{prefix}:definedName>"));
        }
        if section.close_start == section.end {
            let open = stripped[section.start..section.open_end].strip_suffix('>').and_then(|s| s.trim_end_matches(jscompat::is_js_space).strip_suffix('/')).ok_or("Malformed definedNames.")?.trim_end_matches(jscompat::is_js_space);
            return Ok(format!("{}{open}>{entries}</{}>{}", &stripped[..section.start], section.name, &stripped[section.end..]));
        }
        return Ok(format!("{}{entries}{}", &stripped[..section.close_start], &stripped[section.close_start..]));
    }
    let sheets = xml::root_elements(&stripped, "sheets")?.into_iter().next().ok_or("workbook.xml has no sheets element.")?;
    let groups = xml::root_elements(&stripped, "functionGroups")?.into_iter().next();
    let external = xml::root_elements(&stripped, "externalReferences")?.into_iter().next();
    let at = external.map(|element| element.end).or_else(|| groups.map(|element| element.end)).unwrap_or(sheets.end);
    let prefix = sheets.name.split_once(':').map_or("", |(prefix, _)| prefix);
    let element = if prefix.is_empty() { "definedNames".to_owned() } else { format!("{prefix}:definedNames") };
    if !prefix.is_empty() {
        entries = entries.replace("<definedName ", &format!("<{prefix}:definedName ")).replace("</definedName>", &format!("</{prefix}:definedName>"));
    }
    Ok(format!("{}<{element}>{entries}</{element}>{}", &stripped[..at], &stripped[at..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use super::super::package::EntrySource;
    struct Source(HashMap<String, String>);
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> { Ok(self.0.keys().cloned().collect()) }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> { self.0.get(path).cloned().map(Arc::new).ok_or_else(|| "missing".into()) }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> { Ok(true) }
        fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String> { Ok(self.0.get(path).is_some_and(|text| text.contains(needle))) }
        fn release_text(&mut self, _: &str) {}
        fn short_date_pattern(&mut self) -> Result<String, String> { Ok("m/d/yyyy".into()) }
    }
    #[test]
    fn clearing_last_mark_removes_part_and_only_its_declarations() {
        let rels = format!("<Relationships><Relationship Id=\"rId1\" Type=\"{REL_TYPE}\" Target=\"gxRedactions.json\"/><Relationship Id=\"rId2\" Type=\"other\" Target=\"other.xml\"/></Relationships>");
        let types = "<Types><Override PartName=\"/xl/gxRedactions.json\" ContentType=\"application/json\"/><Override PartName=\"/other\" ContentType=\"x\"/></Types>";
        let mut source = Source(HashMap::from([(PART.into(), "{}".into()), (RELS.into(), rels), (TYPES.into(), types.into())]));
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        apply_part(&mut pkg, &mut touched, &[]).unwrap();
        let plan = pkg.to_plan(touched);
        assert_eq!(plan.removed_entries, vec![PART]);
        assert!(plan.replaced.iter().any(|(path, text)| path == RELS && text.contains("rId2") && !text.contains(REL_TYPE)));
        assert!(plan.replaced.iter().any(|(path, text)| path == TYPES && text.contains("/other") && !text.contains("/xl/gxRedactions.json")));
    }
    #[test]
    fn mirror_roundtrips_withheld_coordinates_without_exposing_text() {
        let workbook = r#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#;
        let states = vec![json!({"sheetName":"Data","marks":[{"startRow":2,"endRow":4,"startColumn":0,"endColumn":1,"label":"private"}]})];
        let written = apply_mirror(workbook, &states, &["Data".into()]).unwrap();
        let read = file_parse::xlsx::workbook_redaction_mirror(&written).unwrap();
        assert_eq!(read[0]["marks"][0]["startRow"], 2);
        assert_eq!(read[0]["marks"][0]["endColumn"], 1);
    }
    #[test]
    fn copied_mark_keeps_previous_fill_and_uses_copy_operations() {
        let states = vec![json!({"sheetName":"Old","marks":[{"startRow":1,"endRow":1,"startColumn":0,"endColumn":0,"label":"secret","previousFill":"#abc"}]})];
        let plan = json!({"renames":[{"sheetName":"Old","newName":"New"}],"additions":[{"name":"Copy","sourceSheetName":"Old"}]});
        let ops = vec![json!({"sheetName":"Copy","ops":[{"kind":"insert-rows","index":0,"count":2}]})];
        let result = rekey(&states, Some(&plan), &ops).unwrap();
        assert_eq!(result[0]["sheetName"], "New");
        assert_eq!(result[1]["marks"][0]["startRow"], 3);
        assert_eq!(result[1]["marks"][0]["previousFill"], "#abc");
    }
}
