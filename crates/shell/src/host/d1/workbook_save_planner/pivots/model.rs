use serde_json::Value;
use super::{AxisLine, CacheField, CalculatedCacheField, LabelOp, PivotFilter};
use super::table::{PivotTable, PivotValue};

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] { value.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) }
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> { value.get(key).and_then(Value::as_str).ok_or_else(|| format!("Pivot is missing {key}.")) }
fn index(value: &Value) -> Result<usize, String> {
    let value = value.as_u64().ok_or("Invalid pivot index.")?;
    usize::try_from(value).map_err(|_| "Invalid pivot index.".to_owned())
}
fn indices(value: &Value, key: &str) -> Result<Vec<usize>, String> { array(value, key).iter().map(index).collect() }
fn strings(value: &Value) -> Result<Vec<String>, String> {
    value.as_array().ok_or("Invalid pivot items.")?.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(|| "Invalid pivot item.".into())).collect()
}
fn levels(value: &Value, key: &str) -> Result<Vec<Vec<String>>, String> { array(value, key).iter().map(strings).collect() }
fn hidden(value: &Value, key: &str) -> Result<Vec<Vec<usize>>, String> {
    array(value, key).iter().map(|v| v.as_array().ok_or_else(|| "Invalid pivot hidden items.".to_owned())?.iter().map(index).collect()).collect()
}
fn lines(value: &Value, key: &str, defaults: &[String]) -> Result<Vec<AxisLine>, String> {
    if value.get(key).is_none_or(Value::is_null) {
        return Ok(defaults.iter().enumerate().map(|(index, _)| AxisLine { kind: "data".into(), members: vec![index] }).collect());
    }
    array(value, key).iter().map(|line| Ok(AxisLine { kind: text(line, "t")?.to_owned(), members: indices(line, "members")? })).collect()
}
fn column_label(mut col: usize) -> String {
    let mut chars = Vec::new();
    loop { chars.push((b'A' + (col % 26) as u8) as char); if col < 26 { break; } col = col / 26 - 1; }
    chars.iter().rev().collect()
}
fn area_index(value: &Value, key: &str) -> Result<usize, String> { index(value.get(key).ok_or_else(|| format!("Pivot area is missing {key}."))?) }
pub(super) fn area_bounds(value: &Value) -> Result<(usize, usize, usize, usize), String> {
    Ok((area_index(value, "startRow")?, area_index(value, "startColumn")?, area_index(value, "endRow")?, area_index(value, "endColumn")?))
}
fn area_ref(value: &Value) -> Result<String, String> {
    let (r1, c1, r2, c2) = area_bounds(value)?;
    Ok(format!("{}{}:{}{}", column_label(c1), r1 + 1, column_label(c2), r2 + 1))
}

pub(super) struct Model<'a> {
    addition: &'a Value,
    pub fields: Vec<String>,
    rows: Vec<usize>, columns: Vec<usize>, pages: Vec<usize>,
    row_items: Vec<Vec<String>>, col_items: Vec<Vec<String>>, page_items: Vec<Vec<String>>,
    row_lines: Vec<AxisLine>, col_lines: Vec<AxisLine>,
    row_hidden: Vec<Vec<usize>>, col_hidden: Vec<Vec<usize>>, page_selected: Vec<Option<usize>>,
    calculated: Vec<(String, String)>,
}
impl<'a> Model<'a> {
    pub fn parse(addition: &'a Value) -> Result<Self, String> {
        super::validation::validate(addition)?;
        Self::parse_unchecked(addition)
    }
    pub fn parse_unchecked(addition: &'a Value) -> Result<Self, String> {
        let fields = strings(addition.get("fieldNames").ok_or("Pivot is missing fieldNames.")?)?;
        let rows = indices(addition, "rowFieldIndices")?;
        let columns = if addition.get("columnFieldIndices").is_some_and(|v| !v.is_null()) { indices(addition, "columnFieldIndices")? }
            else { addition.get("columnFieldIndex").filter(|v| !v.is_null()).map(index).transpose()?.into_iter().collect() };
        let pages = indices(addition, "pageFieldIndices")?;
        let default_rows = strings(addition.get("rowItems").ok_or("Pivot is missing rowItems.")?)?;
        let default_cols = addition.get("columnItems").filter(|v| !v.is_null()).map(strings).transpose()?.unwrap_or_default();
        let row_items = if addition.get("rowLevelItems").is_some_and(|v| !v.is_null()) { levels(addition, "rowLevelItems")? } else { vec![default_rows.clone()] };
        let col_items = if addition.get("colLevelItems").is_some_and(|v| !v.is_null()) { levels(addition, "colLevelItems")? } else if addition.get("columnItems").is_some_and(|v| !v.is_null()) { vec![default_cols.clone()] } else { Vec::new() };
        let page_items = levels(addition, "pageLevelItems")?;
        let row_lines = lines(addition, "rowLines", &default_rows)?;
        let col_lines = lines(addition, "colLines", &default_cols)?;
        let row_hidden = hidden(addition, "rowHiddenItems")?;
        let col_hidden = hidden(addition, "colHiddenItems")?;
        let page_selected = array(addition, "pageItems").iter().map(|v| if v.is_null() { Ok(None) } else { index(v).map(Some) }).collect::<Result<_,_>>()?;
        let mut calculated = Vec::new();
        for value in array(addition, "values") {
            if let Some(formula) = value.get("formula").and_then(Value::as_str) {
                calculated.push((value.get("calcName").and_then(Value::as_str).unwrap_or("").to_owned(), super::formula::normalize(formula, &fields)?));
            }
        }
        Ok(Self { addition, fields, rows, columns, pages, row_items, col_items, page_items, row_lines, col_lines, row_hidden, col_hidden, page_selected, calculated })
    }
    pub fn cache_xml(&self, records_id: &str, count: usize) -> Result<String, String> {
        let mut items = vec![None; self.fields.len()];
        for (level, field) in self.rows.iter().enumerate() { items[*field] = Some(self.row_items.get(level).map_or(&[][..], Vec::as_slice)); }
        for (level, field) in self.columns.iter().enumerate() { items[*field] = Some(self.col_items.get(level).map_or(&[][..], Vec::as_slice)); }
        for (level, field) in self.pages.iter().enumerate() {
            if let Some(list) = self.page_items.get(level) { items[*field] = Some(list); }
            else if items[*field].is_none() { items[*field] = Some(&[]); }
        }
        let fields: Vec<_> = self.fields.iter().enumerate().map(|(index, name)| CacheField { name, items: items[index] }).collect();
        let calculated: Vec<_> = self.calculated.iter().map(|(name, formula)| CalculatedCacheField { name, formula }).collect();
        Ok(super::build_cache_definition(records_id, &area_ref(&self.addition["sourceArea"])?, text(self.addition, "sourceSheetName")?, &fields, &calculated, count))
    }
    pub fn table_xml(&self, cache_id: u64, short_date: &str) -> Result<String, String> {
        let mut values = Vec::new();
        for value in array(self.addition, "values") {
            let calculated_name = if value.get("formula").is_some_and(|v| !v.is_null()) { Some(value.get("calcName").and_then(Value::as_str).unwrap_or("")) } else { None };
            values.push(PivotValue { field_index: if calculated_name.is_some() { 0 } else { index(&value["fieldIndex"])? }, aggregation: text(value, "agg")?,
                caption: value.get("name").and_then(Value::as_str), number_format: value.get("numFmt").and_then(Value::as_str),
                show_data_as: value.get("showDataAs").and_then(Value::as_str), base_field: value.get("baseFieldIndex").map(index).transpose()?,
                base_item: value.get("baseItemIndex").map(index).transpose()?, calculated_name });
        }
        let mut filters = Vec::new();
        for filter in array(self.addition, "filters") {
            let field = filter["field"].as_f64().ok_or("Invalid pivot filter field.")?;
            match (text(filter, "kind")?, text(filter, "op")?) {
                ("label", op) => filters.push(PivotFilter::Label { field, op: match op { "equal" => LabelOp::Equal, "contains" => LabelOp::Contains, _ => LabelOp::BeginsWith }, value: text(filter, "value")? }),
                ("value", "top") => filters.push(PivotFilter::Top { field, data_field: filter["dataField"].as_f64().ok_or("Invalid pivot filter data field.")?, count: filter["count"].as_f64().unwrap_or(10.0) }),
                ("value", "greaterThan") => filters.push(PivotFilter::Greater { field, data_field: filter["dataField"].as_f64().ok_or("Invalid pivot filter data field.")?, from: filter["from"].as_f64().unwrap_or(0.0) }),
                ("value", _) => filters.push(PivotFilter::Between { field, data_field: filter["dataField"].as_f64().ok_or("Invalid pivot filter data field.")?, from: filter["from"].as_f64().unwrap_or(0.0), to: filter["to"].as_f64().unwrap_or(0.0) }),
                _ => return Err("Invalid pivot filter.".into()),
            }
        }
        let location = area_ref(&self.addition["location"])?;
        super::table::build_pivot_table(cache_id, &PivotTable { name: text(self.addition, "name")?, location: &location, field_names: &self.fields,
            row_fields: &self.rows, row_items: &self.row_items, row_lines: &self.row_lines, row_hidden: &self.row_hidden,
            column_fields: &self.columns, column_items: &self.col_items, column_lines: &self.col_lines, column_hidden: &self.col_hidden,
            page_fields: &self.pages, page_items: &self.page_items, page_selected: &self.page_selected, values: &values, filters: &filters,
            groupings: array(self.addition, "groupings"), short_date })
    }
    pub fn source_area(&self) -> Result<(usize, usize, usize, usize), String> { area_bounds(&self.addition["sourceArea"]) }
    pub fn source_sheet(&self) -> Result<&str, String> { text(self.addition, "sourceSheetName") }
    pub fn dimensions(&self) -> (&[usize], &[Vec<String>], &[usize], &[Vec<String>], &[usize], &[Vec<String>]) {
        (&self.rows, &self.row_items, &self.columns, &self.col_items, &self.pages, &self.page_items)
    }
    pub fn groupings(&self) -> &[Value] { array(self.addition, "groupings") }
}
