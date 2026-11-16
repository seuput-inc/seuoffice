use super::{
    package::PackageEditor,
    state::PlannerState,
    tables::{self, Area},
    xml,
};
use serde_json::Value;
use std::collections::HashSet;

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        _ => v.to_string(),
    }
}
fn columns(filter: &Value, offset: i64, width: Option<i64>) -> Result<String, String> {
    let mut cols = filter
        .get("columns")
        .and_then(Value::as_array)
        .ok_or("Filter columns are missing.")?
        .iter()
        .collect::<Vec<_>>();
    cols.sort_by_key(|v| v.get("colId").and_then(Value::as_i64).unwrap_or(0));
    let mut out = String::new();
    for col in cols {
        let id = col
            .get("colId")
            .and_then(Value::as_i64)
            .ok_or("Filter column needs colId.")?
            + offset;
        if id < 0 || width.is_some_and(|w| id >= w) {
            return Err(if width.is_some() {
                "A filter column falls outside its table and cannot be saved."
            } else {
                "A filter column falls outside its range and cannot be saved."
            }
            .into());
        }
        let mut body = String::new();
        if col.get("values").is_some_and(|v| !v.is_null())
            || col.get("blank").and_then(Value::as_bool) == Some(true)
        {
            let blank = if col.get("blank").and_then(Value::as_bool) == Some(true) {
                " blank=\"1\""
            } else {
                ""
            };
            let mut values = String::new();
            if let Some(items) = col.get("values").and_then(Value::as_array) {
                for item in items {
                    values.push_str(&format!(
                        "<filter val=\"{}\"/>",
                        tables::escape(&text(item))
                    ));
                }
            }
            body.push_str(&format!("<filters{blank}>{values}</filters>"));
        }
        if let Some(custom) = col.get("customs").filter(|v| !v.is_null()) {
            let and = if custom.get("and").and_then(Value::as_bool) == Some(true) {
                " and=\"1\""
            } else {
                ""
            };
            let mut filters = String::new();
            for item in custom
                .get("filters")
                .and_then(Value::as_array)
                .ok_or("Custom filters are missing.")?
            {
                let operator = item
                    .get("operator")
                    .and_then(Value::as_str)
                    .unwrap_or("equal");
                if !matches!(
                    operator,
                    "equal"
                        | "notEqual"
                        | "greaterThan"
                        | "greaterThanOrEqual"
                        | "lessThan"
                        | "lessThanOrEqual"
                ) {
                    return Err(format!(
                        "Filter condition \"{operator}\" cannot be saved as XLSX yet."
                    ));
                }
                let attr = if operator == "equal" {
                    String::new()
                } else {
                    format!(" operator=\"{operator}\"")
                };
                filters.push_str(&format!(
                    "<customFilter{attr} val=\"{}\"/>",
                    tables::escape(&text(item.get("val").unwrap_or(&Value::Null)))
                ));
            }
            body.push_str(&format!("<customFilters{and}>{filters}</customFilters>"));
        }
        if !body.is_empty() {
            out.push_str(&format!(
                "<filterColumn colId=\"{id}\">{body}</filterColumn>"
            ));
        }
    }
    Ok(out)
}
fn filter_element(
    range: Area,
    filter: Option<&Value>,
    offset: i64,
    width: Option<i64>,
) -> Result<String, String> {
    let cols = match filter {
        Some(v) => columns(v, offset, width)?,
        None => String::new(),
    };
    Ok(if cols.is_empty() {
        format!("<autoFilter ref=\"{}\"/>", range.reference())
    } else {
        format!(
            "<autoFilter ref=\"{}\">{cols}</autoFilter>",
            range.reference()
        )
    })
}
fn insert_after_data(xml_text: &str, element: &str) -> Result<String, String> {
    let mut end = None;
    for name in [
        "sheetData",
        "sheetProtection",
        "protectedRanges",
        "scenarios",
    ] {
        if let Some(e) = tables::first(xml_text, name)? {
            end = Some(end.unwrap_or(0usize).max(e.end));
        }
    }
    let at = end.ok_or("Worksheet has no sheetData element.")?;
    Ok(tables::replace_range(xml_text, at, at, element))
}
fn visibility(xml_text: &str, range: Area, hidden: &HashSet<i64>) -> Result<String, String> {
    let mut result = xml_text.to_owned();
    let rows = xml::root_elements(xml_text, "row")?;
    let mut seen = HashSet::new();
    for row in rows.iter().rev() {
        let Some(number) = row.attribute("r").and_then(|v| v.parse::<i64>().ok()) else {
            continue;
        };
        let index = number - 1;
        if index <= range.r1 || index > range.r2 {
            continue;
        }
        seen.insert(index);
        let mut attrs = row.attributes.clone();
        attrs.retain(|(k, _)| k != "hidden");
        if hidden.contains(&index) {
            attrs.push(("hidden".into(), "1".into()));
        }
        let attr = attrs
            .iter()
            .map(|(k, v)| format!(" {k}=\"{}\"", tables::escape(v)))
            .collect::<String>();
        let self_closing = row.open_end == row.end;
        let opener = format!(
            "<{}{attr}{}>",
            row.name,
            if self_closing { "/" } else { "" }
        );
        result = tables::replace_range(&result, row.start, row.open_end, &opener);
    }
    let mut missing = hidden
        .iter()
        .copied()
        .filter(|r| *r > range.r1 && *r <= range.r2 && !seen.contains(r))
        .collect::<Vec<_>>();
    missing.sort_unstable();
    for row in missing {
        let number = row + 1;
        let item = format!("<row r=\"{number}\" hidden=\"1\"/>");
        let mut at = None;
        for existing in xml::root_elements(&result, "row")? {
            if existing
                .attribute("r")
                .and_then(|n| n.parse::<i64>().ok())
                .is_some_and(|n| n > number)
            {
                at = Some(existing.start);
                break;
            }
        }
        if let Some(at) = at {
            result = tables::replace_range(&result, at, at, &item);
        } else {
            let data = tables::first(&result, "sheetData")?
                .ok_or("Worksheet has no sheetData element.")?;
            if data.open_end == data.end {
                result = tables::replace_range(
                    &result,
                    data.start,
                    data.end,
                    &format!("<sheetData>{item}</sheetData>"),
                );
            } else {
                result = tables::replace_range(&result, data.close_start, data.close_start, &item);
            }
        }
    }
    Ok(result)
}
fn apply_state(
    pkg: &mut PackageEditor<'_>,
    worksheet: &str,
    state: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let filter = state.get("filter").filter(|v| !v.is_null());
    let visibility_range = Area::from_value(
        state
            .get("visibilityRange")
            .ok_or("Filter needs visibilityRange.")?,
    )?;
    let match_area = match filter {
        Some(f) => Area::from_value(f.get("range").ok_or("Filter needs range.")?)?,
        None => visibility_range,
    };
    let hidden = state
        .get("hiddenRows")
        .and_then(Value::as_array)
        .ok_or("Filter needs hiddenRows.")?
        .iter()
        .map(|v| v.as_i64().ok_or_else(|| "Invalid hidden row.".to_owned()))
        .collect::<Result<HashSet<_>, _>>()?;
    let mut route = None;
    for table in tables::sheet_tables(pkg, worksheet)? {
        let Some(root) = tables::first(&table.xml, "table")? else {
            continue;
        };
        let Some(area) = root
            .attribute("ref")
            .and_then(|r| tables::parse_ref(r).ok())
        else {
            continue;
        };
        if area.contains(match_area) {
            let header = root
                .attribute("headerRowCount")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(1);
            let totals = root
                .attribute("totalsRowCount")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            route = Some((table, area, header, totals));
        } else if area.overlaps(match_area) {
            return Err(format!(
                "The filter range overlaps Excel table {} without sitting inside it — filter within the table, or clear the table first.",
                area.reference()
            ));
        }
    }
    let sheet = pkg.read_text(worksheet)?;
    if let Some((table, mut area, header, totals)) = route {
        let mut xml_text = table.xml.clone();
        let old = tables::first(&xml_text, "autoFilter")?;
        let element = if header == 0 {
            String::new()
        } else {
            area.r2 -= totals;
            filter_element(
                area,
                filter,
                match_area.c1 - area.c1,
                Some(area.c2 - area.c1 + 1),
            )?
        };
        if let Some(old) = old {
            xml_text = tables::replace_range(&xml_text, old.start, old.end, &element);
        } else if !element.is_empty() {
            let root = tables::first(&xml_text, "table")?.ok_or("Malformed table.")?;
            xml_text = tables::replace_range(&xml_text, root.open_end, root.open_end, &element);
        }
        pkg.write(&table.path, xml_text)?;
        touched.insert(table.path);
        pkg.write(worksheet, visibility(&sheet, visibility_range, &hidden)?)?;
    } else {
        let element = match filter {
            Some(f) => filter_element(match_area, Some(f), 0, None)?,
            None => String::new(),
        };
        let mut xml_text = sheet.as_ref().clone();
        if let Some(old) = tables::first(&xml_text, "autoFilter")? {
            xml_text = tables::replace_range(&xml_text, old.start, old.end, &element);
        } else if !element.is_empty() {
            xml_text = insert_after_data(&xml_text, &element)?;
        }
        pkg.write(worksheet, visibility(&xml_text, visibility_range, &hidden)?)?;
    }
    touched.insert(worksheet.into());
    Ok(())
}
pub(super) fn apply(
    pkg: &mut PackageEditor<'_>,
    state: &mut PlannerState,
    resolved: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    if let Some(states) = resolved.get("filterStates").and_then(Value::as_array) {
        for filter in states {
            let name = filter
                .get("sheetName")
                .and_then(Value::as_str)
                .ok_or("Filter needs sheetName.")?;
            let path = state.path_for(pkg, name)?;
            apply_state(pkg, &path, filter, touched)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn serializes_value_and_custom_filter_columns() {
        let filter = json!({"columns":[{"colId":1,"customs":{"and":true,"filters":[{"operator":"greaterThan","val":10}]}},{"colId":0,"values":["A&B"],"blank":true}]});
        let xml = filter_element(
            Area {
                r1: 0,
                c1: 0,
                r2: 9,
                c2: 1,
            },
            Some(&filter),
            0,
            None,
        )
        .unwrap();
        assert_eq!(
            xml,
            "<autoFilter ref=\"A1:B10\"><filterColumn colId=\"0\"><filters blank=\"1\"><filter val=\"A&amp;B\"/></filters></filterColumn><filterColumn colId=\"1\"><customFilters and=\"1\"><customFilter operator=\"greaterThan\" val=\"10\"/></customFilters></filterColumn></autoFilter>"
        );
        assert!(columns(&json!({"columns":[{"colId":0,"customs":{"filters":[{"operator":"bogus","val":0}]}}]}),0,None).is_err());
    }
    #[test]
    fn updates_visibility_and_creates_missing_rows() {
        let sheet = "<worksheet><sheetData><row r=\"1\"/><row r=\"2\" hidden=\"1\"/><row r=\"4\"/></sheetData></worksheet>";
        let result = visibility(
            sheet,
            Area {
                r1: 0,
                c1: 0,
                r2: 3,
                c2: 0,
            },
            &HashSet::from([2]),
        )
        .unwrap();
        assert!(result.contains("<row r=\"2\"/>"));
        assert!(result.contains("<row r=\"3\" hidden=\"1\"/>"));
        assert!(result.find("r=\"3\"").unwrap() < result.find("r=\"4\"").unwrap());
    }
    #[test]
    fn routes_table_filter_and_updates_sheet_visibility() {
        use super::super::package::EntrySource;
        use std::{collections::HashMap, sync::Arc};
        struct Source(HashMap<String, String>);
        impl EntrySource for Source {
            fn paths(&mut self) -> Result<Vec<String>, String> {
                Ok(self.0.keys().cloned().collect())
            }
            fn read_text(&mut self, p: &str) -> Result<Arc<String>, String> {
                self.0
                    .get(p)
                    .cloned()
                    .map(Arc::new)
                    .ok_or_else(|| format!("missing {p}"))
            }
            fn can_patch(&mut self, _: &str) -> Result<bool, String> {
                Ok(true)
            }
            fn contains_text(&mut self, p: &str, n: &str) -> Result<bool, String> {
                Ok(self.0.get(p).is_some_and(|v| v.contains(n)))
            }
            fn short_date_pattern(&mut self) -> Result<String, String> {
                Ok("m/d/yyyy".into())
            }
            fn release_text(&mut self, _: &str) {}
        }
        let mut source=Source(HashMap::from([
            ("xl/worksheets/sheet1.xml".into(),"<worksheet><sheetData><row r=\"1\"/><row r=\"2\"/></sheetData></worksheet>".into()),
            ("xl/worksheets/_rels/sheet1.xml.rels".into(),"<Relationships><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/table\" Target=\"../tables/table1.xml\"/></Relationships>".into()),
            ("xl/tables/table1.xml".into(),"<table name=\"Sales\" ref=\"A1:C4\" totalsRowCount=\"1\"><autoFilter ref=\"A1:C3\"/></table>".into()),
        ]));
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        let snapshot = json!({"filter":{"range":{"startRow":0,"startColumn":1,"endRow":2,"endColumn":2},"columns":[{"colId":0,"values":["x"]}]},"visibilityRange":{"startRow":0,"startColumn":0,"endRow":2,"endColumn":2},"hiddenRows":[1]});
        apply_state(
            &mut pkg,
            "xl/worksheets/sheet1.xml",
            &snapshot,
            &mut touched,
        )
        .unwrap();
        let table = pkg.read_text("xl/tables/table1.xml").unwrap();
        assert!(table.contains("<autoFilter ref=\"A1:C3\">"));
        assert!(table.contains("<filterColumn colId=\"1\">"));
        let sheet = pkg.read_text("xl/worksheets/sheet1.xml").unwrap();
        assert!(sheet.contains("<row r=\"2\" hidden=\"1\"/>"));
        assert!(!sheet.contains("<autoFilter"));
        assert!(touched.contains("xl/tables/table1.xml"));
    }
}
