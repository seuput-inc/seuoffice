use jscompat::TryJsRegex;

fn attributes(name: &str) -> Option<&'static str> {
    Some(match name {
        "pivotTableDefinition" => "name cacheId applyNumberFormats applyBorderFormats applyFontFormats applyPatternFormats applyAlignmentFormats applyWidthHeightFormats dataCaption updatedVersion createdVersion minRefreshableVersion useAutoFormatting itemPrintTitles compact compactData outline outlineData multipleFieldFilters",
        "location" => "ref firstHeaderRow firstDataRow firstDataCol rowPageCount colPageCount",
        "pivotFields" | "items" | "rowFields" | "colFields" | "rowItems" | "colItems" | "pageFields" | "dataFields" | "filters" | "cacheFields" | "sharedItems" => "count",
        "pivotField" => "axis showAll compact outline dataField",
        "item" => "x h t", "field" => "x", "i" => "r t i", "x" | "s" => "v",
        "pageField" => "fld item hier", "dataField" => "name fld subtotal showDataAs baseField baseItem numFmtId",
        "pivotTableStyleInfo" => "name showRowHeaders showColHeaders showRowStripes showColStripes showLastColumn",
        "filter" => "fld type evalOrder id stringValue1 iMeasureFld", "autoFilter" => "ref",
        "filterColumn" => "colId", "customFilters" => "and", "customFilter" => "operator val",
        "top10" => "val", "extLst" => "", "ext" => "uri", "aio:aioPivotGroupings" => "v",
        "pivotCacheDefinition" => "r:id refreshedBy createdVersion refreshedVersion minRefreshableVersion recordCount refreshOnLoad",
        "cacheSource" => "type", "worksheetSource" => "ref sheet", "cacheField" => "name numFmtId formula databaseField",
        _ => return None,
    })
}

fn attr_value(raw: &str, name: &str) -> Result<Option<String>, String> {
    let regex = TryJsRegex::new(&format!(r#"\b{name}="([^"]*)""#), "").map_err(|e| e.to_string())?;
    let found = regex.try_find_iter(raw).next().transpose().map_err(|e| e.to_string())?;
    Ok(found.and_then(|found| found.get(1).map(str::to_owned)))
}

pub(super) fn assert_modelled(xml: &str, part: &str) -> Result<(), String> {
    let tags = TryJsRegex::new(r#"<(\/?)([A-Za-z_][\w.:-]*)((?:"[^"]*"|'[^']*'|[^>"'])*?)(\/?)>"#, "").map_err(|e| e.to_string())?;
    let attrs = TryJsRegex::new(r#"([A-Za-z_][\w.:-]*)\s*="# , "").map_err(|e| e.to_string())?;
    let unmodelled = |detail: String| format!("The pivot {part} has {detail}, which a layout edit cannot preserve. Re-create the pivot after changing its layout.");
    for tag in tags.try_find_iter(xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if tag.get(1) == Some("/") { continue; }
        let name = tag.get(2).unwrap_or("");
        let raw = tag.get(3).unwrap_or("");
        let Some(allowed) = attributes(name) else { return Err(unmodelled(format!("an unmodelled <{name}> section"))); };
        for attribute in attrs.try_find_iter(raw) {
            let attribute = attribute.map_err(|e| e.to_string())?;
            let key = attribute.get(1).unwrap_or("");
            if key == "xmlns" || key.starts_with("xmlns:") { continue; }
            if !allowed.split(' ').any(|item| item == key) {
                return Err(unmodelled(format!("an unmodelled <{name} {key}=\"…\"> attribute")));
            }
        }
        if name == "pivotTableStyleInfo" {
            if let Some(style) = attr_value(raw, "name")? {
                if style != "PivotStyleLight16" { return Err(unmodelled(format!("the pivot table style \"{style}\""))); }
            }
        }
        if name == "dataField" {
            let mode = attr_value(raw, "showDataAs")?;
            let base_modes = ["percent", "percentOfParent", "difference", "percentDiff", "runTotal", "percentOfRunningTotal", "rankAscending", "rankDescending"];
            let item_modes = ["percent", "difference", "percentDiff"];
            for (key, allowed) in [("baseField", &base_modes[..]), ("baseItem", &item_modes[..])] {
                if let Some(value) = attr_value(raw, key)? {
                    if value != "0" && !mode.as_deref().is_some_and(|mode| allowed.contains(&mode)) {
                        return Err(unmodelled(format!("a dataField {key}")));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn apply_parts<Table, Cache>(
    pkg: &mut super::super::package::PackageEditor<'_>, table_path: &str, table_xml: &str, cache_path: &str,
    touched: &mut std::collections::HashSet<String>, table_builder: Table, cache_builder: Cache,
) -> Result<(), String>
where
    Table: FnOnce(&str, u64) -> Result<String, String>,
    Cache: FnOnce(&str) -> Result<String, String>,
{
    let cache_xml = pkg.read_text(cache_path)?;
    assert_modelled(table_xml, "table")?;
    assert_modelled(&cache_xml, "cache")?;
    let root = TryJsRegex::new(r#"<pivotTableDefinition\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    let found = root.try_find_iter(table_xml).next().transpose().map_err(|e| e.to_string())?;
    let root = found.as_ref().map(|found| found.as_str()).unwrap_or("");
    let name = attr_value(root, "name")?.unwrap_or_default().replace("&quot;", "\"").replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&");
    let cache_id = attr_value(root, "cacheId")?.and_then(|value| value.parse::<u64>().ok());
    let Some(cache_id) = cache_id.filter(|_| !name.is_empty()) else {
        return Err("The pivot table definition is missing its name or cacheId.".into());
    };
    pkg.write(table_path, table_builder(&name, cache_id)?)?;
    touched.insert(table_path.to_owned());
    let root = TryJsRegex::new(r#"<pivotCacheDefinition\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    let found = root.try_find_iter(&cache_xml).next().transpose().map_err(|e| e.to_string())?;
    let root = found.as_ref().map(|found| found.as_str()).unwrap_or("");
    let records_id = attr_value(root, "r:id")?.ok_or("The pivot cache definition is missing its records relationship.")?;
    pkg.write(cache_path, cache_builder(&records_id)?)?;
    touched.insert(cache_path.to_owned());
    let rels = super::rels_path_for(cache_path);
    if pkg.has(&rels)? {
        let xml = pkg.read_text(&rels)?;
        let tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
        for tag in tags.try_find_iter(&xml) {
            let tag = tag.map_err(|e| e.to_string())?;
            if !attr_value(tag.as_str(), "Type")?.is_some_and(|kind| kind.contains("pivotCacheRecords")) { continue; }
            let Some(target) = attr_value(tag.as_str(), "Target")? else { continue };
            let path = super::super::state::resolve_rel_target(cache_path, &target)?;
            pkg.write(&path, "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<pivotCacheRecords xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" count=\"0\"/>".to_owned())?;
            touched.insert(path);
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::assert_modelled;
    #[test]
    fn rejects_unmodelled_content_before_rebuilding() {
        assert!(assert_modelled(r#"<pivotTableDefinition name="P"><pivotFields count="0"/></pivotTableDefinition>"#, "table").is_ok());
        assert_eq!(assert_modelled(r#"<pivotTableDefinition name="P"><pivotTableStyleInfo name="Custom"/></pivotTableDefinition>"#, "table").unwrap_err(), "The pivot table has the pivot table style \"Custom\", which a layout edit cannot preserve. Re-create the pivot after changing its layout.");
    }
}
