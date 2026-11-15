//! Worksheet naming rules shared by sheet management and formula rewriting.

pub(super) fn validate_sheet_name(name: &str) -> Result<(), String> {
    let length = name.encode_utf16().count();
    if length == 0 || length > 31 {
        return Err(format!("Sheet name \"{name}\" must be 1-31 characters long."));
    }
    if name.chars().any(|c| matches!(c, '\\' | '/' | '?' | '*' | '[' | ']' | ':')) {
        return Err(format!("Sheet name \"{name}\" contains a forbidden character (\\ / ? * [ ] :)."));
    }
    if name.starts_with('\'') || name.ends_with('\'') {
        return Err(format!("Sheet name \"{name}\" cannot start or end with an apostrophe."));
    }
    Ok(())
}

pub(super) fn format_sheet_qualifier(name: &str) -> String {
    let bytes = name.as_bytes();
    let identifier = bytes.first().is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        && bytes.iter().all(|c| c.is_ascii_alphanumeric() || matches!(*c, b'_' | b'.'));
    let letters = bytes.iter().take_while(|c| c.is_ascii_alphabetic()).count();
    let cell = (1..=3).contains(&letters) && letters < bytes.len() && bytes[letters..].iter().all(u8::is_ascii_digit);
    let upper = name.to_ascii_uppercase();
    let r1c1 = upper.strip_prefix('R').and_then(|s| {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        s[n..].strip_prefix('C')
    }).is_some_and(|s| s.bytes().all(|c| c.is_ascii_digit()));
    if !identifier || cell || matches!(upper.as_str(), "TRUE" | "FALSE") || r1c1 {
        format!("'{}'", name.replace('\'', "''"))
    } else {
        name.to_owned()
    }
}

pub(super) fn next_free_relationship_id(xml: &str) -> String {
    let mut used = std::collections::HashSet::new();
    if let Some(root) = super::style_entries::children(xml).into_iter().next() {
        for relationship in super::style_entries::children(root.inner) {
            if let Some(id) = super::styles::attr(relationship.xml, "Id") { used.insert(id.to_owned()); }
        }
    }
    let mut n = 1usize;
    loop {
        let id = format!("rId{n}");
        if !used.contains(&id) { return id; }
        n += 1;
    }
}

pub(super) const REFERENCE: &str = concat!(
    "(^|[^\\p{L}\\p{N}_.$'!:])",
    "(?:('(?:[^']|'')+'|[\\p{L}_][\\p{L}\\p{N}_.]*)!)?",
    "(\\$?[A-Z]{1,3}\\$?[0-9]+(?::\\$?[A-Z]{1,3}\\$?[0-9]+)?",
    "|\\$?[A-Z]{1,3}:\\$?[A-Z]{1,3}|\\$?[0-9]+:\\$?[0-9]+)",
    "(?![0-9\\p{L}_(])"
);

pub(super) fn qualifier_matches(qualifier: &str, name: &str) -> bool {
    let unquoted = qualifier.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))
        .map(|s| s.replace("''", "'")).unwrap_or_else(|| qualifier.to_owned());
    unquoted.to_lowercase() == name.to_lowercase()
}

pub(super) fn rename_sheet_in_formula(formula: &str, old_name: &str, new_name: &str) -> Result<String, String> {
    let regex = jscompat::TryJsRegex::new(REFERENCE, "gu").map_err(|e| e.to_string())?;
    let mut result = String::new();
    for (index, part) in formula.split('"').enumerate() {
        if index != 0 { result.push('"'); }
        if index % 2 == 1 { result.push_str(part); continue; }
        let rewritten = regex.try_replace_all(part, |m| {
            if m.get(2).is_some_and(|q| qualifier_matches(q, old_name)) {
                format!("{}{}!{}", m.get(1).unwrap_or(""), format_sheet_qualifier(new_name), m.get(3).unwrap_or(""))
            } else { m.as_str().to_owned() }
        }).map_err(|e| e.to_string())?;
        result.push_str(&rewritten);
    }
    Ok(result)
}

pub(super) fn formula_references_sheet(formula: &str, name: &str) -> Result<bool, String> {
    let regex = jscompat::TryJsRegex::new(REFERENCE, "gu").map_err(|e| e.to_string())?;
    for (index, part) in formula.split('"').enumerate() {
        if index % 2 == 1 { continue; }
        for hit in regex.try_find_iter(part) {
            let hit = hit.map_err(|e| e.to_string())?;
            if hit.get(2).is_some_and(|q| qualifier_matches(q, name)) { return Ok(true); }
        }
    }
    Ok(false)
}

pub(super) fn descendants<'a>(xml: &'a str, found: &mut Vec<super::style_entries::Element<'a>>) {
    for element in super::style_entries::children(xml) {
        found.push(element);
        if !element.inner.is_empty() { descendants(element.inner, found); }
    }
}

pub(super) fn element_text(inner: &str) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    while let Some(relative) = inner[cursor..].find('<') {
        let at = cursor + relative;
        result.push_str(&super::cells::decode_xml(&inner[cursor..at]));
        if inner[at..].starts_with("<![CDATA[") {
            let Some(end) = inner[at + 9..].find("]]>") else { return result };
            result.push_str(&inner[at + 9..at + 9 + end]);
            cursor = at + 9 + end + 3;
        } else if inner[at..].starts_with("<!--") {
            let Some(end) = inner[at + 4..].find("-->") else { return result };
            cursor = at + 4 + end + 3;
        } else {
            let Some(end) = inner[at..].find('>') else { return result };
            cursor = at + end + 1;
        }
    }
    result.push_str(&super::cells::decode_xml(&inner[cursor..]));
    result
}

fn split_location(location: &str) -> Option<(&str, &str)> {
    if !location.starts_with('\'') { return location.split_once('!'); }
    let bytes = location.as_bytes();
    let mut at = 1;
    while at < bytes.len() {
        if bytes[at] == b'\'' {
            if bytes.get(at + 1) == Some(&b'\'') { at += 2; continue; }
            if bytes.get(at + 1) == Some(&b'!') { return Some((&location[..=at], &location[at + 2..])); }
            return None;
        }
        at += 1;
    }
    None
}

pub(super) fn rename_sheet_references(xml: &str, old_name: &str, new_name: &str, chart: bool) -> Result<String, String> {
    let mut elements = Vec::new();
    descendants(xml, &mut elements);
    let mut replacements = Vec::new();
    for element in elements {
        let local = element.name.rsplit(':').next().unwrap_or(element.name);
        let formula = if chart { element.name == "c:f" } else { matches!(local, "f" | "formula" | "formula1" | "formula2" | "definedName") };
        if formula && !element.inner.is_empty() {
            let text = element_text(element.inner);
            let renamed = rename_sheet_in_formula(&text, old_name, new_name)?;
            if renamed != text {
                let start = element.inner.as_ptr() as usize - xml.as_ptr() as usize;
                replacements.push((start, start + element.inner.len(), super::cells::xml_text(&renamed)));
            }
        }
        if !chart && local == "hyperlink" {
            if let Some(location) = super::styles::attr(element.xml, "location") {
                let location = super::cells::decode_xml(location);
                if let Some((qualifier, target)) = split_location(&location) {
                    if qualifier_matches(qualifier, old_name) {
                        let updated = set_attribute(element.xml, "location", Some(&format!("{}!{target}", format_sheet_qualifier(new_name))));
                        let start = element.xml.as_ptr() as usize - xml.as_ptr() as usize;
                        replacements.push((start, start + element.xml.len(), updated));
                    }
                }
            }
        }
    }
    let mut result = xml.to_owned();
    replacements.sort_unstable_by_key(|(start, _, _)| *start);
    for (start, end, replacement) in replacements.into_iter().rev() { result.replace_range(start..end, &replacement); }
    Ok(result)
}

pub(super) fn set_attribute(xml: &str, name: &str, value: Option<&str>) -> String {
    let bytes = xml.as_bytes();
    let mut at = xml.find(char::is_whitespace).unwrap_or(xml.len());
    while at < bytes.len() && bytes[at] != b'>' {
        let whitespace = at;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() { at += 1; }
        if at >= bytes.len() || matches!(bytes[at], b'/' | b'>') { break; }
        let start = at;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() && !matches!(bytes[at], b'=' | b'/' | b'>') { at += 1; }
        if start == at { at += 1; continue; }
        let key = &xml[start..at];
        while at < bytes.len() && bytes[at].is_ascii_whitespace() { at += 1; }
        if bytes.get(at) != Some(&b'=') { continue; }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() { at += 1; }
        let Some(&quote) = bytes.get(at) else { break };
        if !matches!(quote, b'\'' | b'"') { break; }
        at += 1;
        while at < bytes.len() && bytes[at] != quote { at += 1; }
        if at == bytes.len() { break; }
        at += 1;
        if key == name {
            let replacement = value.map(|v| format!("{name}=\"{}\"", super::styles::esc(v))).unwrap_or_default();
            let begin = if value.is_none() { whitespace } else { start };
            return format!("{}{}{}", &xml[..begin], replacement, &xml[at..]);
        }
    }
    if let Some(value) = value {
        let end = xml.find(|c: char| c.is_whitespace() || c == '/' || c == '>').unwrap_or(xml.len());
        format!("{} {name}=\"{}\"{}", &xml[..end], super::styles::esc(value), &xml[end..])
    } else { xml.to_owned() }
}

fn name_is(name: &str, expected: &str) -> bool { name.rsplit(':').next() == Some(expected) }

pub(super) fn apply_sheet_plan_to_workbook(
    workbook: &str,
    plan: &serde_json::Value,
    additions: &[super::state::SheetAllocation],
) -> Result<String, String> {
    use std::collections::{HashMap, HashSet};
    let root = super::style_entries::children(workbook).into_iter().next().ok_or("The workbook has no sheets element.")?;
    let sections = super::style_entries::children(root.inner);
    let sheets = sections.iter().find(|e| name_is(e.name, "sheets")).ok_or("The workbook has no sheets element.")?;
    let existing: Vec<_> = super::style_entries::children(sheets.inner).into_iter().filter(|e| name_is(e.name, "sheet")).filter_map(|e| {
        let name = super::styles::attr(e.xml, "name")?;
        let state = super::styles::attr(e.xml, "state");
        Some((super::cells::decode_xml(name), e.xml.to_owned(), matches!(state, Some("hidden" | "veryHidden"))))
    }).collect();
    let original: Vec<_> = existing.iter().map(|(name, _, _)| name.clone()).collect();
    let mut renames = HashMap::new();
    for rename in plan.get("renames").and_then(serde_json::Value::as_array).into_iter().flatten() {
        let name = rename.get("sheetName").and_then(serde_json::Value::as_str).ok_or("Invalid sheet rename.")?;
        let next = rename.get("newName").and_then(serde_json::Value::as_str).ok_or("Invalid sheet rename.")?;
        validate_sheet_name(next)?;
        if !original.iter().any(|n| n == name) { return Err(format!("Sheet \"{name}\" was not found in the workbook.")); }
        renames.insert(name.to_owned(), next.to_owned());
    }
    for addition in additions { validate_sheet_name(&addition.name)?; }
    let mut removed = HashSet::new();
    for value in plan.get("removals").and_then(serde_json::Value::as_array).into_iter().flatten() {
        let name = value.as_str().ok_or("Invalid sheet removal.")?;
        if !original.iter().any(|n| n == name) { return Err(format!("Sheet \"{name}\" was not found in the workbook.")); }
        if renames.contains_key(name) { return Err(format!("Sheet \"{name}\" cannot be renamed and removed in one save.")); }
        removed.insert(name.to_owned());
    }
    let mut final_sheets: HashMap<String, (String, bool)> = HashMap::new();
    for (name, xml, hidden) in existing {
        if removed.contains(&name) { continue; }
        let final_name = renames.get(&name).unwrap_or(&name);
        let xml = if final_name == &name { xml } else { set_attribute(&xml, "name", Some(final_name)) };
        if final_sheets.insert(final_name.clone(), (xml, hidden)).is_some() {
            return Err("Two sheets would end up with the same name — aborted.".into());
        }
    }
    for addition in additions {
        let xml = format!("<sheet name=\"{}\" sheetId=\"{}\" r:id=\"{}\"/>", super::styles::esc(&addition.name), addition.sheet_id, super::styles::esc(&addition.relationship_id));
        if final_sheets.insert(addition.name.clone(), (xml, false)).is_some() { return Err("Two sheets would end up with the same name — aborted.".into()); }
    }
    for change in plan.get("hiddenChanges").and_then(serde_json::Value::as_array).into_iter().flatten() {
        let name = change.get("sheetName").and_then(serde_json::Value::as_str).ok_or("Invalid sheet visibility change.")?;
        let hidden = change.get("hidden").and_then(serde_json::Value::as_bool).ok_or("Invalid sheet visibility change.")?;
        let final_name = renames.get(name).map(String::as_str).unwrap_or(name);
        let (xml, current) = final_sheets.get_mut(final_name).ok_or_else(|| format!("Sheet \"{name}\" was not found for a visibility change."))?;
        if !(hidden && super::styles::attr(xml, "state") == Some("veryHidden")) {
            *xml = set_attribute(xml, "state", hidden.then_some("hidden"));
        }
        *current = hidden;
    }
    let order = plan.get("order").and_then(serde_json::Value::as_array).ok_or("The sheet order does not match the final sheet set.")?;
    if order.len() != final_sheets.len() || order.iter().any(|n| n.as_str().is_none_or(|n| !final_sheets.contains_key(n))) {
        return Err("The sheet order does not match the final sheet set.".into());
    }
    if order.iter().all(|name| name.as_str().and_then(|n| final_sheets.get(n)).is_some_and(|(_, hidden)| *hidden)) {
        return Err("A workbook needs at least one visible sheet.".into());
    }
    let mut inner = String::new();
    for name in order { if let Some((xml, _)) = name.as_str().and_then(|n| final_sheets.get(n)) { inner.push_str(xml); } }
    let start = sheets.inner.as_ptr() as usize - workbook.as_ptr() as usize;
    let mut result = format!("{}{}{}", &workbook[..start], inner, &workbook[start + sheets.inner.len()..]);
    let map_index = |old: usize| -> Option<usize> {
        let name = original.get(old)?;
        if removed.contains(name) { return None; }
        let final_name = renames.get(name).unwrap_or(name);
        order.iter().position(|value| value.as_str() == Some(final_name.as_str()))
    };
    let root = super::style_entries::children(&result).into_iter().next().ok_or("The workbook has no sheets element.")?;
    if let Some(names) = super::style_entries::children(root.inner).into_iter().find(|e| name_is(e.name, "definedNames")) {
        let mut changed = String::new();
        let mut cursor = 0;
        for name in super::style_entries::children(names.inner).into_iter().filter(|e| name_is(e.name, "definedName")) {
            let start = name.xml.as_ptr() as usize - names.inner.as_ptr() as usize;
            changed.push_str(&names.inner[cursor..start]);
            if let Some(id) = super::styles::attr(name.xml, "localSheetId") {
                if let Some(mapped) = id.parse::<usize>().ok().and_then(&map_index) {
                    changed.push_str(&set_attribute(name.xml, "localSheetId", Some(&mapped.to_string())));
                }
            } else { changed.push_str(name.xml); }
            cursor = start + name.xml.len();
        }
        changed.push_str(&names.inner[cursor..]);
        let at = names.xml.as_ptr() as usize - result.as_ptr() as usize;
        if changed.trim().is_empty() {
            let end = at + names.xml.len();
            result.replace_range(at..end, "");
        }
        else {
            let start = names.inner.as_ptr() as usize - result.as_ptr() as usize;
            let end = start + names.inner.len();
            result.replace_range(start..end, &changed);
        }
    }
    let root = super::style_entries::children(&result).into_iter().next().ok_or("The workbook has no sheets element.")?;
    if let Some(views) = super::style_entries::children(root.inner).into_iter().find(|e| name_is(e.name, "bookViews")) {
        if let Some(view) = super::style_entries::children(views.inner).into_iter().find(|e| name_is(e.name, "workbookView")) {
            if let Some(old) = super::styles::attr(view.xml, "activeTab").and_then(|s| s.parse::<usize>().ok()) {
                let fallback = order.iter().position(|name| name.as_str().and_then(|n| final_sheets.get(n)).is_some_and(|(_, hidden)| !hidden)).unwrap_or(0);
                let updated = set_attribute(view.xml, "activeTab", Some(&map_index(old).unwrap_or(fallback).to_string()));
                let at = view.xml.as_ptr() as usize - result.as_ptr() as usize;
                let end = at + view.xml.len();
                result.replace_range(at..end, &updated);
            }
        }
    }
    if !additions.is_empty() && !result.contains("xmlns:r=") {
        if let Some(at) = result.find("<workbook") { result.insert_str(at + "<workbook".len(), " xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\""); }
    }
    Ok(result)
}

#[derive(Clone)]
struct Relationship {
    kind: String,
    target: String,
    external: bool,
}

fn relationships(xml: &str) -> Vec<Relationship> {
    let mut result = Vec::new();
    let mut elements = Vec::new();
    descendants(xml, &mut elements);
    for element in elements.into_iter().filter(|element| element.name == "Relationship" && element.xml.ends_with("/>")) {
        let Some(kind) = super::styles::attr(element.xml, "Type") else { continue };
        let Some(target) = super::styles::attr(element.xml, "Target") else { continue };
        result.push(Relationship {
            kind: super::cells::decode_xml(kind),
            target: super::cells::decode_xml(target),
            external: super::styles::attr(element.xml, "TargetMode") == Some("External"),
        });
    }
    result
}
fn rels_path(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}
fn rels_owner(path: &str) -> Option<String> {
    let (dir, name) = path.rsplit_once("/_rels/").or_else(|| path.strip_prefix("_rels/").map(|name| ("", name)))?;
    let file = name.strip_suffix(".rels")?;
    Some(if dir.is_empty() { file.to_owned() } else { format!("{dir}/{file}") })
}
fn direct_part(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix).is_some_and(|part| !part.contains('/') && part.ends_with(".xml"))
}
fn classify_owned(rels: &str, sheet_name: &str) -> Result<Vec<String>, String> {
    let mut targets = Vec::new();
    for entry in relationships(rels) {
        if entry.external || entry.kind.ends_with("/hyperlink") || entry.kind.ends_with("/printerSettings") { continue; }
        if ["drawing", "vmlDrawing", "comments", "threadedComment", "ctrlProp", "table", "image"].iter().any(|kind| entry.kind.ends_with(&format!("/{kind}"))) {
            targets.push(entry.target);
            continue;
        }
        let kind = if entry.kind.ends_with("/pivotTable") { "a pivot table".to_owned() } else { format!("a part this build cannot delete safely ({})", entry.kind) };
        return Err(format!("Sheet \"{sheet_name}\" carries {kind} — deleting it is not supported yet."));
    }
    Ok(targets)
}
fn references_sheet(xml: &str, name: &str, chart: bool, skip_scopes: Option<&std::collections::HashSet<usize>>) -> Result<bool, String> {
    let mut elements = Vec::new();
    descendants(xml, &mut elements);
    for element in elements {
        let matches = if chart { element.name == "c:f" } else { matches!(element.name, "f" | "formula" | "formula1" | "formula2" | "definedName") };
        if !matches || element.inner.is_empty() { continue; }
        if element.name == "definedName" {
            if super::styles::attr(element.xml, "localSheetId").and_then(|s| s.parse::<usize>().ok()).is_some_and(|id| skip_scopes.is_some_and(|set| set.contains(&id))) { continue; }
        }
        if formula_references_sheet(&element_text(element.inner), name)? { return Ok(true); }
    }
    Ok(false)
}
fn pivot_sources_sheet(xml: &str, name: &str) -> bool {
    let mut elements = Vec::new();
    descendants(xml, &mut elements);
    elements.into_iter().filter(|element| element.name == "worksheetSource")
        .filter_map(|element| super::styles::attr(element.xml, "sheet"))
        .any(|sheet| super::cells::decode_xml(sheet).to_lowercase() == name.to_lowercase())
}
fn rename_pivot_source(xml: &str, old: &str, new: &str) -> String {
    let mut elements = Vec::new();
    descendants(xml, &mut elements);
    let mut changes = Vec::new();
    for element in elements.into_iter().filter(|element| element.name == "worksheetSource") {
        if super::styles::attr(element.xml, "sheet").is_some_and(|sheet| super::cells::decode_xml(sheet).to_lowercase() == old.to_lowercase()) {
            let at = element.xml.as_ptr() as usize - xml.as_ptr() as usize;
            changes.push((at, at + element.xml.len(), set_attribute(element.xml, "sheet", Some(new))));
        }
    }
    let mut result = xml.to_owned();
    for (start, end, replacement) in changes.into_iter().rev() { result.replace_range(start..end, &replacement); }
    result
}
fn remove_element(xml: &str, name: &str, attr_name: &str, value: &str) -> String {
    let mut elements = Vec::new();
    descendants(xml, &mut elements);
    if let Some(element) = elements.into_iter().find(|element| element.name == name && super::styles::attr(element.xml, attr_name) == Some(value) && element.xml.ends_with("/>")) {
        let start = element.xml.as_ptr() as usize - xml.as_ptr() as usize;
        let mut end = start + element.xml.len();
        while xml.as_bytes().get(end).is_some_and(u8::is_ascii_whitespace) { end += 1; }
        return format!("{}{}", &xml[..start], &xml[end..]);
    }
    xml.to_owned()
}

pub(super) fn apply_sheet_plan(
    pkg: &mut super::package::PackageEditor<'_>,
    state: &mut super::state::PlannerState,
    plan: &serde_json::Value,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    use std::collections::{HashMap, HashSet};
    let workbook = state.workbook_xml.as_str();
    let mut elements = Vec::new();
    descendants(workbook, &mut elements);
    let root_name = elements.first().map(|e| e.name).unwrap_or("workbook");
    let root_prefix = root_name.rsplit_once(':').map(|(prefix, _)| prefix).unwrap_or("");
    let id_pattern = jscompat::TryJsRegex::new(r#"([A-Za-z_][\w.-]*:id)\s*=\s*(?:"([^"]*)"|'([^']*)')"#, "").map_err(|e| e.to_string())?;
    let mut sheet_elements = Vec::new();
    for element in elements.into_iter().filter(|e| name_is(e.name, "sheet") && e.name.rsplit_once(':').map(|(prefix, _)| prefix).unwrap_or("") == root_prefix) {
        let Some(name) = super::styles::attr(element.xml, "name") else { continue };
        let mut id = super::styles::attr(element.xml, "r:id").map(super::cells::decode_xml);
        if id.is_none() {
            if let Some(found) = id_pattern.try_exec(element.xml).map_err(|e| e.to_string())? {
                id = found.get(2).or_else(|| found.get(3)).map(super::cells::decode_xml);
            }
        }
        sheet_elements.push((super::cells::decode_xml(name), id));
    }
    let original: HashSet<_> = sheet_elements.iter().map(|(name, _)| name.as_str()).collect();
    let renames = plan.get("renames").and_then(serde_json::Value::as_array).ok_or("Invalid sheet renames.")?;
    let removals = plan.get("removals").and_then(serde_json::Value::as_array).ok_or("Invalid sheet removals.")?;
    let mut rename_map = HashMap::new();
    for rename in renames {
        let name = rename.get("sheetName").and_then(serde_json::Value::as_str).ok_or("Invalid sheet rename.")?;
        let new = rename.get("newName").and_then(serde_json::Value::as_str).ok_or("Invalid sheet rename.")?;
        validate_sheet_name(new)?;
        if !original.contains(name) { return Err(format!("Sheet \"{name}\" was not found in the workbook.")); }
        rename_map.insert(name, new);
    }
    for addition in &state.additions { validate_sheet_name(&addition.name)?; }
    let mut removed_names = HashSet::new();
    for removal in removals {
        let name = removal.as_str().ok_or("Invalid sheet removal.")?;
        if !original.contains(name) { return Err(format!("Sheet \"{name}\" was not found in the workbook.")); }
        if rename_map.contains_key(name) { return Err(format!("Sheet \"{name}\" cannot be renamed and removed in one save.")); }
        removed_names.insert(name);
    }
    let mut final_names = HashSet::new();
    for (name, _) in &sheet_elements {
        if removed_names.contains(name.as_str()) { continue; }
        let final_name = rename_map.get(name.as_str()).copied().unwrap_or(name);
        if !final_names.insert(final_name.to_owned()) { return Err("Two sheets would end up with the same name — aborted.".into()); }
    }
    for addition in &state.additions {
        if !final_names.insert(addition.name.clone()) { return Err("Two sheets would end up with the same name — aborted.".into()); }
    }
    let mut removal_paths = HashMap::new();
    for removal in removals {
        let name = removal.as_str().ok_or("Invalid sheet removal.")?;
        removal_paths.insert(name, super::state::resolve_worksheet_path(pkg, name)?);
    }
    let path_set: HashSet<_> = removal_paths.values().cloned().collect();
    let package_paths = pkg.paths()?;
    let worksheets: Vec<_> = package_paths.iter().filter(|path| direct_part(path, "xl/worksheets/") && !path_set.contains(*path)).cloned().collect();
    let charts: Vec<_> = package_paths.iter().filter(|path| path.starts_with("xl/charts/") && path.ends_with(".xml")).cloned().collect();
    let pivots: Vec<_> = package_paths.iter().filter(|path| direct_part(path, "xl/pivotCache/") && path.rsplit('/').next().is_some_and(|p| p.starts_with("pivotCacheDefinition"))).cloned().collect();
    let mut rel_paths = HashMap::new();
    let mut owned_by = HashMap::new();
    let mut removed_owned = HashSet::new();
    for removal in removals {
        let name = removal.as_str().ok_or("Invalid sheet removal.")?;
        let sheet_path = removal_paths.get(name).ok_or("Removed sheet path is missing.")?;
        let path = rels_path(sheet_path);
        rel_paths.insert(name, path.clone());
        let mut owned = HashSet::new();
        if pkg.has(&path)? {
            let mut queue = Vec::new();
            for target in classify_owned(&pkg.read_text(&path)?, name)? {
                queue.push(super::state::resolve_rel_target(sheet_path, &target)?);
            }
            while let Some(part) = queue.pop() {
                if owned.contains(&part) || !pkg.has(&part)? { continue; }
                owned.insert(part.clone());
                let child = rels_path(&part);
                if !pkg.has(&child)? { continue; }
                owned.insert(child.clone());
                for rel in relationships(&pkg.read_text(&child)?) {
                    if !rel.external { queue.push(super::state::resolve_rel_target(&part, &rel.target)?); }
                }
            }
        }
        removed_owned.extend(owned.iter().cloned());
        owned_by.insert(name, owned);
    }
    let dying_rels: HashSet<_> = rel_paths.values().cloned().chain(removed_owned.iter().filter(|path| path.ends_with(".rels")).cloned()).collect();
    let mut keep = Vec::new();
    for path in &package_paths {
        if !path.ends_with(".rels") || dying_rels.contains(path) { continue; }
        let Some(owner) = rels_owner(path) else { continue };
        if path_set.contains(&owner) { continue; }
        for rel in relationships(&pkg.read_text(path)?) {
            if !rel.external {
                let target = super::state::resolve_rel_target(&owner, &rel.target)?;
                if removed_owned.contains(&target) { keep.push(target); }
            }
        }
    }
    while let Some(part) = keep.pop() {
        if !removed_owned.remove(&part) { continue; }
        let rel_path = rels_path(&part);
        if removed_owned.remove(&rel_path) {
            for rel in relationships(&pkg.read_text(&rel_path)?) {
                if !rel.external { keep.push(super::state::resolve_rel_target(&part, &rel.target)?); }
            }
        }
    }
    let removed_ids: HashSet<_> = sheet_elements.iter().enumerate().filter_map(|(index, (name, _))| removed_names.contains(name.as_str()).then_some(index)).collect();
    for removal in removals {
        let name = removal.as_str().ok_or("Invalid sheet removal.")?;
        for path in &worksheets {
            let referenced = if pkg.can_patch(path)? { references_sheet(&pkg.read_text(path)?, name, false, None)? } else { pkg.contains_text(path, name)? };
            if referenced { return Err(format!("Another sheet's formulas reference \"{name}\" — deleting it is not allowed.")); }
        }
        for path in &charts {
            if !removed_owned.contains(path) && references_sheet(&pkg.read_text(path)?, name, true, None)? {
                return Err(format!("A chart reads its data from \"{name}\" — deleting it is not allowed."));
            }
        }
        if references_sheet(workbook, name, false, Some(&removed_ids))? { return Err(format!("A workbook defined name references \"{name}\" — deleting it is not allowed.")); }
        for path in &pivots {
            if pivot_sources_sheet(&pkg.read_text(path)?, name) { return Err(format!("A pivot table reads its source data from \"{name}\" — deleting it is not allowed.")); }
        }
        if let Some(parts) = owned_by.get(name) {
            for part in parts {
                if !removed_owned.contains(part) || !direct_part(part, "xl/tables/") { continue; }
                let xml = pkg.read_text(part)?;
                let root = super::style_entries::children(&xml).into_iter().next();
                let table_name = root.and_then(|root| super::styles::attr(root.xml, "displayName").or_else(|| super::styles::attr(root.xml, "name"))).map(super::cells::decode_xml);
                let Some(table_name) = table_name else { continue };
                let needle = format!("{table_name}[");
                for path in &worksheets {
                    let referenced = if pkg.can_patch(path)? { pkg.read_text(path)?.to_lowercase().contains(&needle.to_lowercase()) } else { pkg.contains_text(path, &needle)? };
                    if referenced { return Err(format!("Another sheet's formulas use table \"{table_name}\" on \"{name}\" — deleting it is not allowed.")); }
                }
                let mut nodes = Vec::new();
                descendants(workbook, &mut nodes);
                for node in nodes.into_iter().filter(|node| node.name == "definedName") {
                    if super::styles::attr(node.xml, "localSheetId").and_then(|s| s.parse::<usize>().ok()).is_some_and(|id| removed_ids.contains(&id)) { continue; }
                    if element_text(node.inner).to_lowercase().contains(&needle.to_lowercase()) { return Err(format!("A workbook defined name uses table \"{table_name}\" on \"{name}\" — deleting it is not allowed.")); }
                }
            }
        }
    }
    for (name, path) in &removal_paths {
        if let Some(rels) = rel_paths.get(name) { if pkg.has(rels)? { pkg.remove(rels); } }
        pkg.remove(path);
    }
    for path in &removed_owned { pkg.remove(path); }
    let mut workbook = state.workbook_xml.to_string();
    for rename in renames {
        let old = rename.get("sheetName").and_then(serde_json::Value::as_str).ok_or("Invalid sheet rename.")?;
        let new = rename.get("newName").and_then(serde_json::Value::as_str).ok_or("Invalid sheet rename.")?;
        for path in &worksheets {
            if !pkg.can_patch(path)? {
                if pkg.contains_text(path, old)? { return Err(format!("{path} references \"{old}\" but is too large to rewrite — renaming this sheet cannot be saved.")); }
                continue;
            }
            let xml = pkg.read_text(path)?;
            let next = rename_sheet_references(&xml, old, new, false)?;
            if next != *xml { pkg.write(path, next)?; touched.insert(path.clone()); }
        }
        for path in &charts {
            if removed_owned.contains(path) { continue; }
            let xml = pkg.read_text(path)?;
            let next = rename_sheet_references(&xml, old, new, true)?;
            if next != *xml { pkg.write(path, next)?; touched.insert(path.clone()); }
        }
        for path in &pivots {
            let xml = pkg.read_text(path)?;
            let next = rename_pivot_source(&xml, old, new);
            if next != *xml { pkg.write(path, next)?; touched.insert(path.clone()); }
        }
        workbook = rename_sheet_references(&workbook, old, new, false)?;
    }
    let rel_path = "xl/_rels/workbook.xml.rels";
    let original_rels = pkg.read_text(rel_path)?;
    let mut rels = original_rels.to_string();
    for removal in removals {
        if let Some(id) = sheet_elements.iter().find(|(name, _)| Some(name.as_str()) == removal.as_str()).and_then(|(_, id)| id.as_deref()) {
            rels = remove_element(&rels, "Relationship", "Id", id);
        }
    }
    if rels != *original_rels { pkg.write(rel_path, rels)?; touched.insert(rel_path.into()); }
    let types_path = "[Content_Types].xml";
    let original_types = pkg.read_text(types_path)?;
    let mut types = original_types.to_string();
    for path in removal_paths.values().chain(removed_owned.iter()) { types = remove_element(&types, "Override", "PartName", &format!("/{path}")); }
    for addition in &state.additions {
        if !types.contains("</Types>") { return Err("The workbook has no [Content_Types].xml Types element.".into()); }
        let entry = format!("<Override PartName=\"/{}\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>", addition.path);
        types = types.replacen("</Types>", &format!("{entry}</Types>"), 1);
    }
    if types != *original_types { pkg.write(types_path, types)?; touched.insert(types_path.into()); }
    state.workbook_xml = std::sync::Arc::new(apply_sheet_plan_to_workbook(&workbook, plan, &state.additions)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_count_utf16_and_reject_forbidden_characters() {
        assert!(validate_sheet_name(&"😀".repeat(15)).is_ok());
        assert!(validate_sheet_name(&"😀".repeat(16)).is_err());
        assert_eq!(validate_sheet_name("bad/name").unwrap_err(), "Sheet name \"bad/name\" contains a forbidden character (\\ / ? * [ ] :).");
    }

    #[test]
    fn qualifiers_quote_cell_names_and_escape_apostrophes() {
        assert_eq!(format_sheet_qualifier("Data_2026"), "Data_2026");
        assert_eq!(format_sheet_qualifier("A12"), "'A12'");
        assert_eq!(format_sheet_qualifier("R1C2"), "'R1C2'");
        assert_eq!(format_sheet_qualifier("Owner's"), "'Owner''s'");
    }
    #[test]
    fn workbook_reorders_scopes_and_clamps_active_tab() {
        let xml = "<workbook xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><bookViews><workbookView activeTab=\"1\"/></bookViews><sheets><sheet name=\"A\" sheetId=\"1\" r:id=\"rId1\"/><sheet name=\"B\" sheetId=\"2\" r:id=\"rId2\"/></sheets><definedNames><definedName name=\"keep\" localSheetId=\"0\">A!A1</definedName><definedName name=\"drop\" localSheetId=\"1\">B!A1</definedName></definedNames></workbook>";
        let plan = serde_json::json!({"renames":[{"sheetName":"A","newName":"First"}],"removals":["B"],"order":["First"],"hiddenChanges":[]});
        let result = apply_sheet_plan_to_workbook(xml, &plan, &[]).unwrap();
        assert!(result.contains("<sheet name=\"First\" sheetId=\"1\" r:id=\"rId1\"/>"));
        assert!(result.contains("name=\"keep\" localSheetId=\"0\""));
        assert!(!result.contains("name=\"drop\""));
        assert!(result.contains("activeTab=\"0\""));
    }
    #[test]
    fn sheet_references_ignore_literals_and_keep_external_qualifiers() {
        let formula = "SUM('Old Name'!A1:B2)+\"'Old Name'!A1\"+Other!A1";
        assert_eq!(rename_sheet_in_formula(formula, "Old Name", "A12").unwrap(), "SUM('A12'!A1:B2)+\"'Old Name'!A1\"+Other!A1");
        assert!(formula_references_sheet(formula, "old name").unwrap());
        assert!(!formula_references_sheet("\"'Old Name'!A1\"", "Old Name").unwrap());
    }
}
