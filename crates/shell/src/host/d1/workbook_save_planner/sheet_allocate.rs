//! Allocate added sheet identities and seed blank or safely cloned parts.
use super::{
    package::PackageEditor,
    state::{PlannerState, SheetAllocation, resolve_worksheet_path},
    xml,
};
use jscompat::TryJsRegex;
use serde_json::Value;
use std::collections::HashSet;

const BLANK: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><dimension ref=\"A1\"/><sheetViews><sheetView workbookViewId=\"0\"/></sheetViews><sheetData/></worksheet>";
const SHEET_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
fn regex(pattern: &str) -> Result<TryJsRegex, String> {
    TryJsRegex::new(pattern, "").map_err(|e| e.to_string())
}
fn replace_with_group(xml: &str, pattern: &str) -> Result<String, String> {
    regex(pattern)?
        .try_replace_all(xml, |m| m.get(1).unwrap_or("").to_owned())
        .map_err(|e| e.to_string())
}
fn rels_path(path: &str) -> Result<String, String> {
    let re = regex(r"^(xl/worksheets/)([^/]+)$")?;
    Ok(match re.try_exec(path).map_err(|e| e.to_string())? {
        Some(m) => format!(
            "{}_rels/{}.rels",
            m.get(1).unwrap_or(""),
            m.get(2).unwrap_or("")
        ),
        None => path.to_owned(),
    })
}
fn clone_rels(rels: &str, name: &str) -> Result<(Option<String>, bool), String> {
    let re = regex(r#"<Relationship\b[^>]*?\bType="([^"]+)"[^>]*?/>\s*"#)?;
    let mut result = String::with_capacity(rels.len());
    let mut cursor = 0;
    let mut dropped = false;
    for found in re.try_find_iter(rels) {
        let found = found.map_err(|e| e.to_string())?;
        let kind = found.get(1).unwrap_or("");
        if !kind.ends_with("/hyperlink") && !kind.ends_with("/printerSettings") {
            return Err(format!(
                "Sheet \"{name}\" carries charts, images, tables, or comments — duplicating it is not supported yet."
            ));
        }
        if kind.ends_with("/printerSettings") {
            result.push_str(&rels[cursor..found.start()]);
            cursor = found.end();
            dropped = true;
        }
    }
    result.push_str(&rels[cursor..]);
    let contains = regex(r"<Relationship\b")?
        .try_test(&result)
        .map_err(|e| e.to_string())?;
    Ok((contains.then_some(result), dropped))
}
fn check_scoped_names(workbook: &str, name: &str) -> Result<(), String> {
    let sheets = xml::root_elements(workbook, "sheet")?;
    let index = sheets
        .iter()
        .filter(|sheet| sheet.attribute("name").is_some())
        .position(|sheet| sheet.attribute("name") == Some(name))
        .ok_or_else(|| format!("Sheet \"{name}\" was not found in the workbook."))?;
    let pattern = format!(r#"<definedName\b[^>]*?\blocalSheetId="{index}""#);
    if regex(&pattern)?
        .try_test(workbook)
        .map_err(|e| e.to_string())?
    {
        return Err(format!(
            "Sheet \"{name}\" has sheet-scoped defined names — duplicating it is not supported yet."
        ));
    }
    Ok(())
}
/// Gateway stages 1238–1273. Allocations are stored in original addition order.
pub(super) fn allocate(
    pkg: &mut PackageEditor<'_>,
    state: &mut PlannerState,
    request: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let Some(plan) = request.get("sheetPlan").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let additions = plan
        .get("additions")
        .and_then(Value::as_array)
        .ok_or("sheetPlan.additions must be an array")?;
    if additions.is_empty() {
        return Ok(());
    }
    let workbook = pkg.read_text("xl/workbook.xml")?;
    let rels = pkg.read_text("xl/_rels/workbook.xml.rels")?;
    let path_re = regex(r"^xl/worksheets/sheet([0-9]+)\.xml$")?;
    let mut next_part = 1u64;
    for path in pkg.paths()? {
        if let Some(found) = path_re.try_exec(&path).map_err(|e| e.to_string())? {
            let number = found
                .get(1)
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or("Worksheet part number overflow.")?;
            next_part = next_part.max(
                number
                    .checked_add(1)
                    .ok_or("Worksheet part number overflow.")?,
            );
        }
    }
    let sheet_ids = regex(r#"<sheet\b(?:"[^"]*"|'[^']*'|[^>"'])*?\bsheetId="([0-9]+)""#)?;
    let mut next_id = 1u64;
    for found in sheet_ids.try_find_iter(&workbook) {
        let found = found.map_err(|e| e.to_string())?;
        let number = found
            .get(1)
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or("Sheet ID overflow.")?;
        next_id = next_id.max(number.checked_add(1).ok_or("Sheet ID overflow.")?);
    }
    let ids = regex(r#"\bId\s*=\s*(["'])rId(\d+)\1"#)?;
    let mut used = HashSet::new();
    for found in ids.try_find_iter(&rels) {
        let found = found.map_err(|e| e.to_string())?;
        if let Some(number) = found.get(2) {
            used.insert(format!("rId{number}"));
        }
    }
    let mut candidate = 1u64;
    let mut updated = rels.as_ref().clone();
    for addition in additions {
        let name = addition
            .get("name")
            .and_then(Value::as_str)
            .ok_or("Added sheet has no name.")?;
        while used.contains(&format!("rId{candidate}")) {
            candidate = candidate
                .checked_add(1)
                .ok_or("Relationship ID overflow.")?;
        }
        let relationship_id = format!("rId{candidate}");
        used.insert(relationship_id.clone());
        let path = format!("xl/worksheets/sheet{next_part}.xml");
        let addition_xml = format!(
            "<Relationship Id=\"{relationship_id}\" Type=\"{SHEET_REL}\" Target=\"{}\"/>",
            &path[3..]
        );
        if !updated.contains("</Relationships>") {
            return Err("The workbook has no relationships part.".into());
        }
        updated = updated.replacen(
            "</Relationships>",
            &format!("{addition_xml}</Relationships>"),
            1,
        );
        state.additions.push(SheetAllocation {
            name: name.into(),
            path,
            sheet_id: next_id,
            relationship_id,
        });
        next_part = next_part
            .checked_add(1)
            .ok_or("Worksheet part number overflow.")?;
        next_id = next_id.checked_add(1).ok_or("Sheet ID overflow.")?;
    }
    pkg.write("xl/_rels/workbook.xml.rels", updated)?;
    touched.insert("xl/_rels/workbook.xml.rels".into());
    for (addition, allocation) in additions.iter().zip(&state.additions) {
        let Some(source) = addition.get("sourceSheetName").and_then(Value::as_str) else {
            pkg.add(&allocation.path, BLANK.into());
            continue;
        };
        let source_path = resolve_worksheet_path(pkg, source)?;
        if !pkg.can_patch(&source_path)? {
            return Err(format!(
                "{source_path} is too large to load — duplicating \"{source}\" cannot be saved."
            ));
        }
        check_scoped_names(&workbook, source)?;
        let xml = pkg.read_text(&source_path)?;
        let mut clone = replace_with_group(&xml, r#"(<sheetView\b[^>]*?)\s+tabSelected="[^"]*""#)?;
        let source_rels_path = rels_path(&source_path)?;
        if pkg.has(&source_rels_path)? {
            let source_rels = pkg.read_text(&source_rels_path)?;
            let (copy, printer) = clone_rels(&source_rels, source)?;
            if printer {
                clone = replace_with_group(&clone, r#"(<pageSetup\b[^>]*?)\s+r:id="[^"]*""#)?;
            }
            if let Some(copy) = copy {
                pkg.add(&rels_path(&allocation.path)?, copy);
            }
        }
        pkg.add(&allocation.path, clone);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::package::EntrySource;
    use super::*;
    use std::{collections::HashMap, sync::Arc};
    struct Source(HashMap<String, String>);
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> {
            Ok(self.0.keys().cloned().collect())
        }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
            self.0
                .get(path)
                .cloned()
                .map(Arc::new)
                .ok_or_else(|| "missing".into())
        }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn contains_text(&mut self, path: &str, text: &str) -> Result<bool, String> {
            Ok(self.0.get(path).is_some_and(|value| value.contains(text)))
        }
        fn release_text(&mut self, _: &str) {}
        fn short_date_pattern(&mut self) -> Result<String, String> {
            Ok("m/d/yyyy".into())
        }
    }
    #[test]
    fn refuses_to_clone_relationships_that_cannot_be_shared() {
        let input = r#"<Relationships><Relationship Id="rId1" Type="http://x/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#;
        assert!(
            clone_rels(input, "A")
                .unwrap_err()
                .contains("duplicating it is not supported")
        );
    }
    #[test]
    fn clone_drops_printer_settings_and_preserves_hyperlinks() {
        let rels = r#"<Relationships><Relationship Id="rId1" Type="http://x/hyperlink" Target="https://example.test"/><Relationship Id="rId2" Type="http://x/printerSettings" Target="../printerSettings/a.bin"/></Relationships>"#;
        let mut source = Source(HashMap::from([
            ("xl/workbook.xml".into(), r#"<workbook><sheets><sheet name="A" sheetId="1" r:id="rId1"/></sheets></workbook>"#.into()),
            ("xl/_rels/workbook.xml.rels".into(), r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#.into()),
            ("xl/worksheets/sheet1.xml".into(), r#"<worksheet><sheetViews><sheetView tabSelected="1"/></sheetViews><sheetData/><pageSetup r:id="rId2"/></worksheet>"#.into()),
            ("xl/worksheets/_rels/sheet1.xml.rels".into(), rels.into()),
        ]));
        let mut pkg = PackageEditor::new(&mut source);
        let mut state = PlannerState::new(&mut pkg).unwrap();
        let mut touched = HashSet::new();
        allocate(
            &mut pkg,
            &mut state,
            &serde_json::json!({"sheetPlan":{"additions":[{"name":"B","sourceSheetName":"A"}]}}),
            &mut touched,
        )
        .unwrap();
        assert_eq!(state.additions[0].path, "xl/worksheets/sheet2.xml");
        let cloned = pkg.read_text("xl/worksheets/sheet2.xml").unwrap();
        assert!(!cloned.contains("tabSelected"));
        assert!(!cloned.contains("r:id=\"rId2\""));
        let cloned_rels = pkg
            .read_text("xl/worksheets/_rels/sheet2.xml.rels")
            .unwrap();
        assert!(cloned_rels.contains("/hyperlink"));
        assert!(!cloned_rels.contains("/printerSettings"));
    }
}
