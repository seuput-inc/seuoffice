//! Pivot table and cache-definition reader, matching xlsx-pivot.ts.
use std::collections::HashMap;
use serde_json::{json, Value};
use shell_ipc_d1::sheets::engine::{engine_error, EngineResult};
use crate::host::d1::{workbook_save_planner, workbook_xml as xml};

fn elements(source: &str, tag: &str) -> EngineResult<Vec<String>> {
    let tag = jscompat::regex_escape(tag);
    xml::matches(&format!(r"<{tag}(?:\s[^>]*)?/>|<{tag}(?:\s[^>]*)?>[\s\S]*?</{tag}>"), "g", source)
}
fn first(source: &str, tag: &str) -> EngineResult<Option<String>> {
    Ok(elements(source, tag)?.into_iter().next())
}
fn attr(source: &str, key: &str) -> EngineResult<Option<String>> { xml::attribute(source, key) }
fn decoded(source: &str, key: &str) -> EngineResult<String> {
    Ok(file_parse::xlsx::workbook_decode_xml_entities(&attr(source, key)?.unwrap_or_default()))
}
fn number(source: &str, key: &str, default: &str) -> EngineResult<f64> {
    Ok(jscompat::to_number(attr(source, key)?.as_deref().unwrap_or(default)))
}
fn items(source: &str) -> EngineResult<Vec<Value>> {
    let Some(block) = first(source, "sharedItems")? else { return Ok(Vec::new()) };
    let tags = xml::matches(r"<(s|n|b|d|m|e)(\s[^>]*)?/?>(?:</\1>)?", "g", &block)?;
    tags.into_iter().map(|tag| {
        let kind = tag.as_bytes().get(1).copied().unwrap_or_default();
        let value = attr(&tag, "v")?;
        Ok(match kind {
            b's' | b'd' => json!(file_parse::xlsx::workbook_decode_xml_entities(value.as_deref().unwrap_or(""))),
            b'n' => json!(jscompat::to_number(value.as_deref().unwrap_or("0"))),
            b'b' => json!(matches!(value.as_deref(), Some("1" | "true"))),
            b'm' => Value::Null,
            _ => json!(file_parse::xlsx::workbook_decode_xml_entities(value.as_deref().unwrap_or("#ERROR"))),
        })
    }).collect()
}
fn valid_grouping(value: &Value) -> bool {
    match value.get("kind").and_then(Value::as_str) {
        Some("date") => matches!(value.get("dateUnit").and_then(Value::as_str), Some("year" | "quarter" | "month")),
        Some("range") => value.get("rangeStep").and_then(Value::as_f64).is_some_and(|n| n.is_finite() && n > 0.0)
            && value.get("rangeStart").is_none_or(|v| v.as_f64().is_some_and(f64::is_finite)),
        _ => false,
    }
}
fn axis(source: &str, tag: &str) -> EngineResult<Vec<f64>> {
    let Some(block) = first(source, tag)? else { return Ok(Vec::new()) };
    elements(&block, "field")?.iter().map(|v| number(v, "x", "0")).collect()
}
fn lines(source: &str, tag: &str, count: usize) -> EngineResult<Vec<Value>> {
    let Some(block) = first(source, tag)? else { return Ok(vec![json!({"t":"data","members":[],"depth":0,"dataField":0})]) };
    let mut current = vec![Value::Null; count];
    let mut data_field = 0.0;
    let mut result = Vec::new();
    for line in elements(&block, "i")? {
        let kind = attr(&line, "t")?.unwrap_or_else(|| "data".into());
        let repeat = number(&line, "r", "0")? as usize;
        let values: Vec<f64> = elements(&line, "x")?.iter().map(|v| number(v, "v", "0")).collect::<EngineResult<_>>()?;
        let depth = repeat.saturating_add(values.len());
        if depth > current.len() { current.resize(depth, Value::Null); }
        for (i, value) in values.into_iter().enumerate() { current[repeat + i] = json!(value); }
        if kind != "data" { for member in current.iter_mut().take(count).skip(depth) { *member = Value::Null; } }
        if let Some(value) = attr(&line, "i")? { data_field = jscompat::to_number(&value); }
        result.push(json!({"t":kind,"members":current,"depth":if kind == "grand" {0} else {depth},"dataField":data_field}));
    }
    Ok(result)
}

pub(super) fn parse(pivot_xml: &str, cache_xml: &str) -> EngineResult<Value> {
    let mut unsupported = Vec::<String>::new();
    let location = first(pivot_xml, "location")?.ok_or_else(|| engine_error("pivotTable part has no location element."))?;
    let output_ref = attr(&location, "ref")?.filter(|s| !s.is_empty()).ok_or_else(|| engine_error("pivotTable location has no ref."))?;
    let first_row = number(&location, "firstDataRow", "1")?;
    let first_col = number(&location, "firstDataCol", "1")?;
    let source = first(cache_xml, "worksheetSource")?;
    if source.is_none() { unsupported.push("the data source is not a worksheet range (external/consolidated source)".into()); }
    let external = if let Some(source) = &source {
        let pos = cache_xml.find(source).unwrap_or(0);
        file_parse::xlsx_formula_redaction::relationship_id_of(source, &cache_xml[..pos]).map_err(|error| engine_error(error.to_string()))?.is_some()
    } else { false };
    let source_sheet = if external { String::new() } else { decoded(source.as_deref().unwrap_or(""), "sheet")? };
    let source_ref = if external { String::new() } else { attr(source.as_deref().unwrap_or(""), "ref")?.unwrap_or_default() };
    if external { unsupported.push("the data source is a range of another workbook".into()); }
    else if source.is_some() && (source_sheet.is_empty() || source_ref.is_empty()) { unsupported.push("the data source uses a defined name, recompute not supported yet".into()); }

    let mut groups = HashMap::<usize, Value>::new();
    if let Some(ext) = xml::first(r#"<(?:\w+:)?aioPivotGroupings\b[^>]*\bv="[^"]*""#, "", pivot_xml)? {
        let raw = decoded(&ext, "v")?;
        let parsed = serde_json::from_str::<Value>(&raw).ok();
        let valid = parsed.as_ref().and_then(Value::as_array).is_some_and(|entries| {
            for entry in entries {
                let Some(mut grouping) = entry.as_object().cloned() else { return false };
                let Some(index) = grouping.remove("fieldIndex").and_then(|v| v.as_f64()) else { return false };
                let grouping = Value::Object(grouping);
                if !valid_grouping(&grouping) { return false; }
                if index >= 0.0 && index.fract() == 0.0 && index <= usize::MAX as f64 {
                    groups.insert(index as usize, grouping);
                }
            }
            true
        });
        if !valid { unsupported.push("grouped-field extension metadata cannot be parsed".into()); }
    }
    let cache_fields = match first(cache_xml, "cacheFields")? { Some(block) => elements(&block, "cacheField")?, None => Vec::new() };
    let mut fields = Vec::new();
    let mut formulas = HashMap::new();
    for (index, field) in cache_fields.iter().enumerate() {
        let mut value = json!({"name":decoded(field,"name")?,"sharedItems":items(field)?});
        if let Some(grouping) = groups.get(&index) { value["grouping"] = grouping.clone(); }
        if let Some(formula) = attr(field, "formula")? {
            let decoded = file_parse::xlsx::workbook_decode_xml_entities(&formula);
            let stripped = decoded.trim_start_matches(jscompat::is_js_space);
            let formula = stripped.strip_prefix('=').map_or(decoded.as_str(), |s| s).to_owned();
            formulas.insert(index, formula.clone());
            value["formula"] = json!(formula);
        }
        fields.push(value);
    }
    let source_names: Vec<String> = fields.iter().enumerate().filter(|(i, _)| !formulas.contains_key(i)).map(|(_, f)| f["name"].as_str().unwrap_or("").to_owned()).collect();
    for index in 0..fields.len() {
        if let Some(formula) = formulas.get(&index) {
            let name = fields[index]["name"].as_str().unwrap_or("");
            if index < source_names.len() { unsupported.push(format!("calculated field \"{name}\" does not come after the source fields, recompute not supported")); }
            else if let Err(error) = workbook_save_planner::validate_pivot_formula(formula, &source_names) { unsupported.push(format!("the formula of calculated field \"{name}\" does not support recompute ({error})")); }
        }
    }
    for (index, field) in cache_fields.iter().enumerate() {
        if first(field, "fieldGroup")?.is_some() { unsupported.push(format!("grouped field \"{}\" does not support recompute", fields[index]["name"].as_str().unwrap_or(""))); }
    }
    let pivot_fields = match first(pivot_xml, "pivotFields")? { Some(block) => elements(&block, "pivotField")?, None => Vec::new() };
    let mut field_items: Vec<Vec<Value>> = Vec::new();
    for field in pivot_fields {
        let mut values = Vec::new();
        if let Some(block) = first(&field, "items")? {
            for item in elements(&block, "item")? {
                if attr(&item, "t")?.is_none() {
                    let x = attr(&item, "x")?.map(|value| json!(jscompat::to_number(&value))).unwrap_or(Value::Null);
                    values.push(json!({"x":x,"hidden":attr(&item,"h")?.as_deref() == Some("1")}));
                }
            }
        }
        field_items.push(values);
    }
    let row_fields = axis(pivot_xml, "rowFields")?;
    let col_fields = axis(pivot_xml, "colFields")?;
    let row_lines = lines(pivot_xml, "rowItems", row_fields.len())?;
    let col_lines = lines(pivot_xml, "colItems", col_fields.len())?;
    let data_xml = match first(pivot_xml, "dataFields")? { Some(block) => elements(&block, "dataField")?, None => Vec::new() };
    let supported_modes = ["percentOfTotal","percentOfRow","percentOfCol","percentOfParentRow","percentOfParentCol","index","percentOf","percentOfParent","difference","percentDiff","runTotal","percentOfRunningTotal","rankAscending","rankDescending"];
    let base_modes = ["percentOf","percentOfParent","difference","percentDiff","runTotal","percentOfRunningTotal","rankAscending","rankDescending"];
    let item_modes = ["percentOf","difference","percentDiff"];
    let mut data_fields = Vec::new();
    for entry in data_xml {
        let raw_mode = attr(&entry, "showDataAs")?;
        let mode = match raw_mode.as_deref() {
            None | Some("normal") => None,
            Some("percent") => Some("percentOf"),
            Some(value) if supported_modes.contains(&value) => Some(value),
            Some(value) => { unsupported.push(format!("\"show values as\" mode \"{value}\" does not support recompute")); None }
        };
        let field = number(&entry, "fld", "0")?;
        let name = decoded(&entry, "name")?;
        let subtotal = attr(&entry, "subtotal")?.unwrap_or_else(|| "sum".into());
        let mut result = json!({"name":name,"field":field,"subtotal":subtotal});
        if let Some(mode) = mode {
            result["showDataAs"] = json!(mode);
            if base_modes.contains(&mode) {
                let base = attr(&entry, "baseField")?.map(|v| jscompat::to_number(&v));
                let valid = base.is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n >= 0.0 && (n as usize) < fields.len() && (row_fields.contains(&n) || col_fields.contains(&n)));
                if !valid {
                    unsupported.push(format!("the base field of data field \"{name}\" does not support recompute"));
                } else if let Some(base) = base {
                    result["baseField"] = json!(base);
                    if item_modes.contains(&mode) {
                        let item = attr(&entry, "baseItem")?.map(|v| jscompat::to_number(&v));
                        if item == Some(1048828.0) || item == Some(1048829.0) {
                            unsupported.push(format!("the \"(previous)\"/\"(next)\" base item of data field \"{name}\" does not support recompute"));
                        } else if !item.is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n >= 0.0 && field_items.get(base as usize).is_some_and(|items| (n as usize) < items.len())) {
                            unsupported.push(format!("the base item of data field \"{name}\" does not support recompute"));
                        } else if let Some(item) = item {
                            result["baseItem"] = json!(item);
                        }
                    }
                }
            }
        }
        if field.is_finite() && field >= 0.0 && field.fract() == 0.0 {
            if let Some(formula) = formulas.get(&(field as usize)) { result["formula"] = json!(formula); }
        }
        data_fields.push(result);
    }
    let aggregations = ["sum","count","average","max","min","product","countNums","stdDev","stdDevp","var","varp"];
    for field in &data_fields {
        let subtotal = field["subtotal"].as_str().unwrap_or("");
        if !aggregations.contains(&subtotal) { unsupported.push(format!("aggregation \"{subtotal}\" does not support recompute")); }
    }
    if data_fields.is_empty() { unsupported.push("this pivot table has no data fields".into()); }

    let mut page_fields = Vec::new();
    if let Some(block) = first(pivot_xml, "pageFields")? {
        for entry in elements(&block, "pageField")? {
            let selected = attr(&entry, "item")?.map(|s| json!(jscompat::to_number(&s))).unwrap_or(Value::Null);
            page_fields.push(json!({"field":number(&entry,"fld","0")?,"item":selected}));
        }
    }
    let positive_count = |key| -> EngineResult<Option<f64>> {
        let n = number(&location, key, "")?;
        Ok((n.is_finite() && n.fract() == 0.0 && n > 0.0).then_some(n))
    };
    let page_rows = positive_count("rowPageCount")?;
    let page_cols = positive_count("colPageCount")?;
    for index in 0..fields.len() {
        if formulas.contains_key(&index) && (row_fields.contains(&(index as f64)) || col_fields.contains(&(index as f64)) || page_fields.iter().any(|page| page["field"].as_f64() == Some(index as f64))) {
            unsupported.push(format!("calculated field \"{}\" is used as a dimension, recompute not supported", fields[index]["name"].as_str().unwrap_or("")));
        }
    }
    let mut filters = Vec::new();
    if let Some(block) = first(pivot_xml, "filters")? {
        for filter in elements(&block, "filter")? {
            let kind = attr(&filter, "type")?.unwrap_or_default();
            let field = number(&filter, "fld", "0")?;
            let data_field = number(&filter, "iMeasureFld", "0")?;
            let custom: Vec<f64> = elements(&filter, "customFilter")?.iter().map(|c| number(c, "val", "NaN")).collect::<EngineResult<_>>()?;
            let label = match kind.as_str() { "captionEqual" => Some("equal"), "captionContains" => Some("contains"), "captionBeginsWith" => Some("beginsWith"), _ => None };
            if let Some(op) = label {
                filters.push(json!({"kind":"label","field":field,"op":op,"value":decoded(&filter,"stringValue1")?}));
            } else if kind == "count" {
                let top = first(&filter, "top10")?;
                let count = match &top { Some(top) => number(top,"val","NaN")?, None => f64::NAN };
                if !count.is_finite() || count < 1.0 || top.as_ref().map(|top| attr(top,"top")).transpose()?.flatten().as_deref() == Some("0") {
                    unsupported.push(format!("value/label filter (pivotFilters) type \"{kind}\" does not support recompute"));
                } else { filters.push(json!({"kind":"value","field":field,"dataField":data_field,"op":"top","count":count})); }
            } else if kind == "valueGreaterThan" && custom.first().is_some_and(|n| n.is_finite()) {
                filters.push(json!({"kind":"value","field":field,"dataField":data_field,"op":"greaterThan","from":custom[0]}));
            } else if kind == "valueBetween" && custom.len() >= 2 && custom[0].is_finite() && custom[1].is_finite() {
                filters.push(json!({"kind":"value","field":field,"dataField":data_field,"op":"between","from":custom[0].min(custom[1]),"to":custom[0].max(custom[1])}));
            } else {
                unsupported.push(format!("value/label filter (pivotFilters) type \"{kind}\" does not support recompute"));
            }
        }
    }
    let mut result = json!({
        "outputRef":output_ref,"firstDataRow":first_row,"firstDataCol":first_col,
        "fields":fields,"fieldItems":field_items,"rowFields":row_fields,"colFields":col_fields,
        "rowLines":row_lines,"colLines":col_lines,"dataFields":data_fields,"pageFields":page_fields,
        "filters":filters,"sourceSheet":source_sheet,"sourceRef":source_ref,"unsupported":unsupported
    });
    if let (Some(rows), Some(cols)) = (page_rows, page_cols) {
        result["pageRows"] = json!(rows); result["pageColumns"] = json!(cols);
    } else if page_rows.is_none() && page_cols.is_none() && result["pageFields"].as_array().is_some_and(Vec::is_empty) {
        result["pageRows"] = json!(0); result["pageColumns"] = json!(0);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_members_repeats_filters_and_source() {
        let pivot = r#"<pivotTableDefinition><location ref="A3:D9" firstDataRow="2" firstDataCol="1"/><pivotFields><pivotField><items><item x="0"/><item x="1" h="1"/></items></pivotField></pivotFields><rowFields><field x="0"/></rowFields><rowItems><i><x v="0"/></i><i r="1" t="default"/></rowItems><dataFields><dataField name="Total" fld="1"/></dataFields><filters><filter type="captionContains" fld="0" stringValue1="A&amp;B"/></filters></pivotTableDefinition>"#;
        let cache = r#"<pivotCacheDefinition><cacheSource><worksheetSource sheet="Sales &amp; Co" ref="A1:B9"/></cacheSource><cacheFields><cacheField name="Region"><sharedItems><s v="East"/><s v="West"/></sharedItems></cacheField><cacheField name="Amount"/></cacheFields></pivotCacheDefinition>"#;
        let result = parse(pivot, cache).unwrap();
        assert_eq!(result["sourceSheet"], "Sales & Co");
        assert_eq!(result["fields"][0]["sharedItems"], json!(["East", "West"]));
        assert_eq!(result["fieldItems"][0][1]["hidden"], true);
        assert_eq!(result["rowLines"][1]["members"], json!([0.0]));
        assert_eq!(result["filters"][0]["value"], "A&B");
        assert_eq!(result["unsupported"], json!([]));
    }
    #[test]
    fn preserves_grouping_and_rejects_previous_base_item() {
        let pivot = r#"<pivotTableDefinition><location ref="A1" rowPageCount="1" colPageCount="1"/><pivotFields><pivotField><items><item x="0"/></items></pivotField></pivotFields><rowFields><field x="0"/></rowFields><dataFields><dataField name="Change" fld="1" showDataAs="difference" baseField="0" baseItem="1048828"/></dataFields><pageFields><pageField fld="0" item="0"/></pageFields><extLst><ext><aioPivotGroupings v="[{&quot;fieldIndex&quot;:0,&quot;kind&quot;:&quot;date&quot;,&quot;dateUnit&quot;:&quot;month&quot;}]"/></ext></extLst></pivotTableDefinition>"#;
        let cache = r#"<pivotCacheDefinition><cacheSource><worksheetSource sheet="Sales" ref="A1:B9"/></cacheSource><cacheFields><cacheField name="Date"><sharedItems><s v="Jan"/></sharedItems></cacheField><cacheField name="Amount"/></cacheFields></pivotCacheDefinition>"#;
        let result = parse(pivot, cache).unwrap();
        assert_eq!(result["fields"][0]["grouping"]["dateUnit"], "month");
        assert_eq!(result["pageRows"], 1.0);
        assert_eq!(result["dataFields"][0]["showDataAs"], "difference");
        assert!(result["unsupported"].as_array().unwrap().iter().any(|v| v.as_str().is_some_and(|s| s.contains("(previous)"))));
    }

    #[test]
    fn preserves_calculated_formula_and_marks_unknown_reference() {
        let pivot = r#"<pivotTableDefinition><location ref="A1"/><dataFields><dataField name="Bad" fld="1"/></dataFields></pivotTableDefinition>"#;
        let cache = r#"<pivotCacheDefinition><cacheSource><worksheetSource sheet="S" ref="A1:A2"/></cacheSource><cacheFields><cacheField name="Amount"/><cacheField name="Bad" formula="=Missing * 2"/></cacheFields></pivotCacheDefinition>"#;
        let result = parse(pivot, cache).unwrap();
        assert_eq!(result["fields"][1]["formula"], "Missing * 2");
        assert_eq!(result["dataFields"][0]["formula"], "Missing * 2");
        assert!(result["unsupported"].as_array().unwrap().iter().any(|v| v.as_str().is_some_and(|s| s.contains("formula"))));
    }

    #[test]
    fn rejects_external_sources_and_unsupported_modes() {
        let pivot = r#"<pivotTableDefinition><location ref="A1"/><dataFields><dataField name="X" fld="0" showDataAs="mystery"/></dataFields></pivotTableDefinition>"#;
        let cache = r#"<pivotCacheDefinition xmlns:x="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><cacheSource><worksheetSource x:id="rId3" sheet="Local" ref="A1:B2"/></cacheSource><cacheFields><cacheField name="A"/></cacheFields></pivotCacheDefinition>"#;
        let result = parse(pivot, cache).unwrap();
        assert_eq!(result["sourceSheet"], "");
        let unsupported = result["unsupported"].as_array().unwrap();
        assert_eq!(unsupported.len(), 2);
    }
}
