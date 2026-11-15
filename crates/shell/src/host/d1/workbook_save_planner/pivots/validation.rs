use std::collections::HashSet;
use serde_json::Value;

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}
fn number(value: &Value) -> f64 { value.as_f64().unwrap_or(f64::NAN) }
fn optional_number(value: &Value, key: &str) -> Option<f64> { value.get(key).and_then(Value::as_f64) }
fn string<'a>(value: &'a Value, key: &str) -> &'a str { value.get(key).and_then(Value::as_str).unwrap_or("") }
fn index_of(values: &[Value], needle: f64) -> Option<usize> { values.iter().position(|value| number(value) == needle) }
fn member_count(levels: &[Value], index: usize) -> usize { levels.get(index).and_then(Value::as_array).map_or(0, Vec::len) }
fn matches_index(value: f64, size: usize) -> bool { value >= 0.0 && value < size as f64 }

pub(super) fn validate(addition: &Value) -> Result<(), String> {
    let name = string(addition, "name");
    let fields = array(addition, "fieldNames");
    let rows = array(addition, "rowFieldIndices");
    let columns_owned;
    let columns = if addition.get("columnFieldIndices").is_some_and(|v| !v.is_null()) {
        array(addition, "columnFieldIndices")
    } else {
        columns_owned = addition.get("columnFieldIndex").filter(|v| !v.is_null()).cloned().into_iter().collect::<Vec<_>>();
        &columns_owned
    };
    let pages = array(addition, "pageFieldIndices");
    let values = array(addition, "values");
    let in_bounds = |v: &Value| matches_index(number(v), fields.len());
    if rows.is_empty() || rows.iter().any(|v| !in_bounds(v)) || columns.iter().any(|v| !in_bounds(v))
        || pages.iter().any(|v| !in_bounds(v)) || values.iter().any(|v| v.get("formula").is_none() && !v.get("fieldIndex").is_some_and(in_bounds)) {
        return Err(format!("Pivot \"{name}\" references a field outside its source."));
    }
    let page_levels = array(addition, "pageLevelItems");
    let page_items = array(addition, "pageItems");
    if (addition.get("pageLevelItems").is_some_and(|v| !v.is_null()) && page_levels.len() != pages.len())
        || (addition.get("pageItems").is_some_and(|v| !v.is_null()) && (page_items.len() != pages.len()
            || page_items.iter().enumerate().any(|(i, v)| !v.is_null() && !matches_index(number(v), member_count(page_levels, i))))) {
        return Err(format!("Pivot \"{name}\" has an inconsistent report filter."));
    }
    let field_names: Vec<String> = fields.iter().filter_map(Value::as_str).map(str::to_owned).collect();
    let mut used_names: HashSet<String> = field_names.iter().map(|name| name.trim().to_lowercase()).collect();
    for value in values.iter().filter(|value| value.get("formula").is_some_and(|v| !v.is_null())) {
        let calc_name = string(value, "calcName").trim();
        if calc_name.is_empty() || !used_names.insert(calc_name.to_lowercase()) || string(value, "agg") != "sum" {
            return Err(format!("Pivot \"{name}\" has an invalid calculated field."));
        }
        if let Err(error) = super::formula::normalize(string(value, "formula"), &field_names) {
            return Err(format!("Pivot \"{name}\" has a bad calculated-field formula: {error}"));
        }
    }
    if !columns.is_empty() && values.len() != 1 {
        return Err(format!("Pivot \"{name}\" with a column field supports exactly one values entry."));
    }
    let row_levels_owned;
    let row_levels = if addition.get("rowLevelItems").is_some_and(|v| !v.is_null()) { array(addition, "rowLevelItems") } else {
        row_levels_owned = vec![addition.get("rowItems").cloned().unwrap_or(Value::Array(Vec::new()))]; &row_levels_owned
    };
    let col_levels_owned;
    let col_levels = if addition.get("colLevelItems").is_some_and(|v| !v.is_null()) { array(addition, "colLevelItems") } else {
        col_levels_owned = addition.get("columnItems").filter(|v| !v.is_null()).cloned().into_iter().collect::<Vec<_>>(); &col_levels_owned
    };
    for value in values {
        let mode = string(value, "showDataAs");
        if !["percentOf", "percentOfParent", "difference", "percentDiff", "runTotal", "percentOfRunningTotal", "rankAscending", "rankDescending"].contains(&mode) { continue; }
        let Some(base) = optional_number(value, "baseFieldIndex").filter(|v| matches_index(*v, fields.len())) else {
            return Err(format!("Pivot \"{name}\" has a missing base field."));
        };
        let row_level = index_of(rows, base);
        let col_level = index_of(columns, base);
        if row_level.is_none() && col_level.is_none() { return Err(format!("Pivot \"{name}\" has a base field outside the row/column dimensions.")); }
        if ["percentOf", "difference", "percentDiff"].contains(&mode) {
            let size = if let Some(level) = row_level { member_count(row_levels, level) } else { member_count(col_levels, col_level.unwrap_or(0)) };
            if !optional_number(value, "baseItemIndex").is_some_and(|v| matches_index(v, size)) {
                return Err(format!("Pivot \"{name}\" has a missing base item."));
            }
        }
    }
    for (axis, fields, levels_key, lines_key) in [("row", rows, "rowLevelItems", "rowLines"), ("column", columns, "colLevelItems", "colLines")] {
        if fields.len() < 2 { continue; }
        let levels = addition.get(levels_key).and_then(Value::as_array);
        let lines = addition.get(lines_key).and_then(Value::as_array);
        let (Some(levels), Some(lines)) = (levels, lines) else { return Err(format!("Pivot \"{name}\" has {} {axis} levels but no per-level items/lines.", fields.len())); };
        if levels.len() != fields.len() { return Err(format!("Pivot \"{name}\" has {} {axis} levels but no per-level items/lines.", fields.len())); }
        for line in lines {
            let members = array(line, "members");
            if members.is_empty() || members.len() > fields.len() || (string(line, "t") == "data" && members.len() != fields.len())
                || members.iter().enumerate().any(|(level, value)| !matches_index(number(value), member_count(levels, level))) {
                return Err(format!("Pivot \"{name}\" has an inconsistent {axis} line."));
            }
        }
    }
    let dimensions: Vec<f64> = rows.iter().chain(columns).map(number).collect();
    for grouping in array(addition, "groupings") {
        let field = optional_number(grouping, "fieldIndex").unwrap_or(f64::NAN);
        let valid = match string(grouping, "kind") {
            "date" => ["year", "quarter", "month"].contains(&string(grouping, "dateUnit")),
            "range" => optional_number(grouping, "rangeStep").is_some_and(|n| n.is_finite() && n > 0.0)
                && grouping.get("rangeStart").map_or(true, |v| v.as_f64().is_some_and(f64::is_finite)),
            _ => false,
        };
        if !dimensions.contains(&field) || !valid { return Err(format!("Pivot \"{name}\" has an invalid field grouping.")); }
    }
    let mut filtered = Vec::new();
    for filter in array(addition, "filters") {
        let field = optional_number(filter, "field").unwrap_or(f64::NAN);
        let valid = match string(filter, "kind") {
            "label" => ["equal", "contains", "beginsWith"].contains(&string(filter, "op")) && filter.get("value").is_some_and(Value::is_string),
            "value" => {
                let data = optional_number(filter, "dataField");
                let op_valid = match string(filter, "op") {
                    "top" => optional_number(filter, "count").is_some_and(|n| n >= 1.0),
                    "greaterThan" => optional_number(filter, "from").is_some(),
                    "between" => optional_number(filter, "from").is_some() && optional_number(filter, "to").is_some(),
                    _ => false,
                };
                data.is_some_and(|n| matches_index(n, values.len())) && op_valid
            }
            _ => false,
        };
        if !valid || !dimensions.contains(&field) || filtered.contains(&field) {
            return Err(format!("Pivot \"{name}\" has an invalid pivot filter."));
        }
        filtered.push(field);
    }
    for (key, levels) in [("rowHiddenItems", row_levels), ("colHiddenItems", col_levels)] {
        for (level, hidden) in array(addition, key).iter().enumerate() {
            if hidden.as_array().is_some_and(|items| items.iter().any(|item| !matches_index(number(item), member_count(levels, level)))) {
                return Err(format!("Pivot \"{name}\" has an out-of-range hidden item."));
            }
        }
    }
    let source = &addition["sourceArea"];
    let width = optional_number(source, "endColumn").unwrap_or(f64::NAN) - optional_number(source, "startColumn").unwrap_or(f64::NAN) + 1.0;
    if width != fields.len() as f64 {
        return Err(format!("Pivot \"{name}\" has {} field names for a {}-column source.", fields.len(), jscompat::number_to_string(width)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate;
    use serde_json::json;

    #[test]
    fn validates_field_bounds_before_later_model_constraints() {
        let pivot = json!({"name":"Sales", "fieldNames":["City","Amount"], "rowFieldIndices":[3],
            "values":[{"fieldIndex":1,"agg":"sum"}], "sourceArea":{"startColumn":0,"endColumn":1}});
        assert_eq!(validate(&pivot).unwrap_err(), "Pivot \"Sales\" references a field outside its source.");
    }

    #[test]
    fn validates_multilevel_axis_and_source_width() {
        let pivot = json!({"name":"Sales", "fieldNames":["City","Amount"], "rowFieldIndices":[0],
            "rowItems":["Paris"], "values":[{"fieldIndex":1,"agg":"sum"}],
            "sourceArea":{"startColumn":0,"endColumn":1}});
        assert!(validate(&pivot).is_ok());
        let mut invalid = pivot;
        invalid["rowFieldIndices"] = json!([0, 1]);
        assert_eq!(validate(&invalid).unwrap_err(), "Pivot \"Sales\" has 2 row levels but no per-level items/lines.");
    }
}
