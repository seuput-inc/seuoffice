//! Shared workbook and worksheet state for interleaved save stages.
use super::package::PackageEditor;
use jscompat::TryJsRegex;
use std::sync::Arc;

pub(super) struct SheetAllocation {
    pub name: String,
    pub path: String,
    pub sheet_id: u64,
    pub relationship_id: String,
}

pub(super) struct Worksheet {
    pub name: String,
    pub path: String,
    pub xml: Arc<String>,
}
pub(super) struct PlannerState {
    /// Added sheets in allocation order, with their workbook identities.
    pub additions: Vec<SheetAllocation>,
    /// Sheets participating in mutation, in first-use order.
    pub worksheets: Vec<Worksheet>,
    pub workbook_original: Arc<String>,
    pub workbook_xml: Arc<String>,
}
impl PlannerState {
    pub fn new(pkg: &mut PackageEditor<'_>) -> Result<Self, String> {
        let original = pkg.read_text("xl/workbook.xml")?;
        Ok(Self {
            additions: Vec::new(),
            worksheets: Vec::new(),
            workbook_original: original.clone(),
            workbook_xml: original,
        })
    }
    pub fn worksheet(&self, name: &str) -> Option<&Worksheet> {
        self.worksheets.iter().find(|sheet| sheet.name == name)
    }
    pub fn worksheet_mut(&mut self, name: &str) -> Option<&mut Worksheet> {
        self.worksheets.iter_mut().find(|sheet| sheet.name == name)
    }
    pub fn path_for(&self, pkg: &mut PackageEditor<'_>, name: &str) -> Result<String, String> {
        if let Some(sheet) = self.additions.iter().find(|sheet| sheet.name == name) {
            return Ok(sheet.path.clone());
        }
        resolve_worksheet_path(pkg, name)
    }
    pub fn load_sheet(&mut self, pkg: &mut PackageEditor<'_>, name: &str) -> Result<(), String> {
        if self.worksheet(name).is_some() {
            return Ok(());
        }
        let path = self.path_for(pkg, name)?;
        let xml = pkg.read_text(&path)?;
        if !xml.contains("<sheetData") {
            return Err(format!(
                "{path} (sheet \"{name}\") has no sheetData element — {} characters read.",
                xml.encode_utf16().count()
            ));
        }
        pkg.release_source_text(&path);
        self.worksheets.push(Worksheet {
            name: name.to_owned(),
            path,
            xml,
        });
        Ok(())
    }
    /// Load every sheet that the later worksheet stages mutate, in gateway order.
    pub fn load_referenced_sheets(
        &mut self,
        pkg: &mut PackageEditor<'_>,
        request: &serde_json::Value,
    ) -> Result<(), String> {
        for field in [
            "edits",
            "formulaValues",
            "bulkConstantFills",
            "structuralOps",
            "filterStates",
            "hyperlinkEdits",
            "cfStates",
            "dvStates",
            "sheetProtections",
            "tabColorStates",
            "pageSetupStates",
            "protectedRangeStates",
        ] {
            match request.get(field) {
                None | Some(serde_json::Value::Null) => {}
                Some(serde_json::Value::Array(values)) => {
                    for value in values {
                        let name = value
                            .get("sheetName")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| format!("{field} has no sheetName"))?;
                        self.load_sheet(pkg, name)?;
                    }
                }
                Some(_) => return Err(format!("{field} must be an array")),
            }
        }
        Ok(())
    }
    /// Commit the in-memory worksheet mutations in their original loading order.
    pub fn flush_worksheets(
        &mut self,
        pkg: &mut PackageEditor<'_>,
        touched: &mut std::collections::HashSet<String>,
    ) -> Result<(), String> {
        for sheet in std::mem::take(&mut self.worksheets) {
            if self
                .additions
                .iter()
                .any(|addition| addition.name == sheet.name)
            {
                pkg.add_shared(&sheet.path, sheet.xml);
            } else {
                pkg.write_shared(&sheet.path, sheet.xml)?;
            }
            touched.insert(sheet.path);
        }
        Ok(())
    }
}
fn regex(pattern: &str) -> Result<TryJsRegex, String> {
    TryJsRegex::new(pattern, "").map_err(|e| e.to_string())
}
fn decode_xml(text: &str) -> Result<String, String> {
    let re = regex(r"&(?:#x([0-9A-Fa-f]+)|#([0-9]+)|(quot|apos|lt|gt|amp));")?;
    re.try_replace_all(text, |m| {
        if let Some(named) = m.get(3) {
            return match named {
                "quot" => "\"",
                "apos" => "'",
                "lt" => "<",
                "gt" => ">",
                "amp" => "&",
                _ => m.as_str(),
            }
            .to_owned();
        }
        let parsed = m
            .get(1)
            .and_then(|v| u32::from_str_radix(v, 16).ok())
            .or_else(|| m.get(2).and_then(|v| v.parse().ok()));
        parsed
            .filter(|n| *n <= 0x10ffff)
            .map(|n| char::from_u32(n).unwrap_or('\u{fffd}').to_string())
            .unwrap_or_else(|| m.as_str().to_owned())
    })
    .map_err(|e| e.to_string())
}
pub(in crate::host::d1) fn resolve_rel_target(
    from_part: &str,
    target: &str,
) -> Result<String, String> {
    let raw = target
        .trim_matches(jscompat::is_js_space)
        .split('#')
        .next()
        .unwrap_or("");
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err("Invalid OPC relationship target.".into());
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| "Invalid OPC relationship target.")?;
    if decoded.is_empty()
        || decoded.contains('\0')
        || decoded.starts_with("//")
        || decoded.starts_with('\\')
        || regex(r"^[A-Za-z][A-Za-z0-9+.-]*:")?
            .try_test(&decoded)
            .map_err(|e| e.to_string())?
    {
        return Err("Invalid OPC relationship target.".into());
    }
    let base = from_part.replace('\\', "/");
    let mut base_parts: Vec<&str> = base.split('/').collect();
    base_parts.pop();
    let mut parts: Vec<String> = if decoded.starts_with('/') {
        Vec::new()
    } else {
        base_parts
            .into_iter()
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_owned)
            .collect()
    };
    for part in decoded.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err("Invalid OPC relationship target.".into());
                }
            }
            part => parts.push(part.to_owned()),
        }
    }
    if parts.is_empty() {
        Err("Invalid OPC relationship target.".into())
    } else {
        Ok(parts.join("/"))
    }
}
/// Resolve the original name through workbook.xml and its relationship part.
pub(super) fn resolve_worksheet_path(
    pkg: &mut PackageEditor<'_>,
    sheet_name: &str,
) -> Result<String, String> {
    let workbook = pkg.read_text("xl/workbook.xml")?;
    let token = regex(
        r#"<!--[\s\S]*?-->|<!\[CDATA\[[\s\S]*?\]\]>|<\?[\s\S]*?\?>|<!DOCTYPE[^>]*>|<(\/?)([^\s/>!?]+)((?:"[^"]*"|'[^']*'|[^>"'])*)>"#,
    )?;
    let attributes = regex(r#"([^\s=<>/"']+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)?;
    let mut root_prefix: Option<String> = None;
    let mut relationship = None;
    for found in token.try_find_iter(&workbook) {
        let found = found.map_err(|e| e.to_string())?;
        let Some(name) = found.get(2) else {
            continue;
        };
        if found.get(1) == Some("/") {
            continue;
        }
        let (prefix, local) = name.split_once(':').unwrap_or(("", name));
        if root_prefix.is_none() {
            root_prefix = Some(prefix.to_owned());
        }
        if local != "sheet" || root_prefix.as_deref() != Some(prefix) {
            continue;
        }
        let mut name_value = None;
        let mut r_id = None;
        let mut other_id = None;
        for attr in attributes.try_find_iter(found.get(3).unwrap_or("")) {
            let attr = attr.map_err(|e| e.to_string())?;
            let key = attr.get(1).unwrap_or("");
            let value = decode_xml(attr.get(2).or_else(|| attr.get(3)).unwrap_or(""))?;
            if key == "name" {
                name_value = Some(value);
            } else if key == "r:id" {
                r_id = Some(value);
            } else if key.ends_with(":id")
                && key.split_once(':').is_some_and(|(prefix, _)| {
                    !prefix.is_empty()
                        && prefix
                            .bytes()
                            .next()
                            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                        && prefix
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                })
                && other_id.is_none()
            {
                other_id = Some(value);
            }
        }
        if name_value.as_deref() == Some(sheet_name) {
            relationship = r_id.or(other_id);
            break;
        }
    }
    let relationship = relationship
        .ok_or_else(|| format!("Sheet \"{sheet_name}\" was not found in workbook.xml."))?;
    let rels = pkg.read_text("xl/_rels/workbook.xml.rels")?;
    let tags = regex(r#"<Relationship\b[^>]*/?>"#)?;
    let ids = regex(r#"\bId="([^"]*)""#)?;
    let targets = regex(r#"\bTarget="([^"]+)""#)?;
    for tag in tags.try_find_iter(&rels) {
        let tag = tag.map_err(|e| e.to_string())?;
        let id = ids.try_exec(tag.as_str()).map_err(|e| e.to_string())?;
        if id.as_ref().and_then(|m| m.get(1)) != Some(relationship.as_str()) {
            continue;
        }
        let target = targets
            .try_exec(tag.as_str())
            .map_err(|e| e.to_string())?
            .and_then(|m| m.get(1))
            .ok_or_else(|| format!("Relationship {relationship} was not found."))?;
        let decoded = decode_xml(target)?;
        let target = decoded
            .strip_prefix("/xl/")
            .or_else(|| decoded.strip_prefix("xl/"))
            .unwrap_or(&decoded);
        return resolve_rel_target("xl/workbook.xml", target);
    }
    Err(format!("Relationship {relationship} was not found."))
}

#[cfg(test)]
mod tests {
    use super::super::package::EntrySource;
    use super::*;
    struct Source;
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> {
            Ok(vec![
                "xl/workbook.xml".into(),
                "xl/_rels/workbook.xml.rels".into(),
            ])
        }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
            Ok(Arc::new(match path {
                "xl/workbook.xml" => "<workbook><sheets><sheet name=\"A&gt;B\" q:id=\"rId7\"/></sheets></workbook>".into(),
                "xl/_rels/workbook.xml.rels" => "<Relationships><Relationship Target=\"worksheets/sheet%201.xml\" Id=\"rId7\"/></Relationships>".into(),
                _ => return Err("missing".into()),
            }))
        }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn contains_text(&mut self, _: &str, _: &str) -> Result<bool, String> {
            Ok(false)
        }
        fn release_text(&mut self, _: &str) {}
        fn short_date_pattern(&mut self) -> Result<String, String> {
            Ok("m/d/yyyy".into())
        }
    }
    #[test]
    fn resolves_decoded_sheet_name_and_unordered_relationship() {
        let mut source = Source;
        let mut pkg = PackageEditor::new(&mut source);
        assert_eq!(
            resolve_worksheet_path(&mut pkg, "A>B").unwrap(),
            "xl/worksheets/sheet 1.xml"
        );
    }
    #[test]
    fn relationship_target_blocks_escape_and_decodes_segments() {
        assert_eq!(
            resolve_rel_target("xl/workbook.xml", "worksheets/sheet%201.xml").unwrap(),
            "xl/worksheets/sheet 1.xml"
        );
        assert!(resolve_rel_target("xl/workbook.xml", "../../outside.xml").is_err());
        assert!(resolve_rel_target("xl/workbook.xml", "https://example.test/x").is_err());
        assert!(resolve_rel_target("xl/workbook.xml", "bad%zz").is_err());
    }
}
