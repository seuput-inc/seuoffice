use jscompat::TryJsRegex;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum SourceValue {
    Missing,
    Number(f64),
    Text(String),
}

fn regex(pattern: &str) -> Result<TryJsRegex, String> {
    TryJsRegex::new(pattern, "").map_err(|e| e.to_string())
}
fn decode(input: &str) -> String {
    input
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn first_group(xml: &str, pattern: &str, group: usize) -> Result<Option<String>, String> {
    let re = regex(pattern)?;
    let found = re
        .try_find_iter(xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?;
    Ok(found.and_then(|found| found.get(group).map(str::to_owned)))
}

pub(super) fn shared_strings(xml: &str) -> Result<Vec<String>, String> {
    let strings = regex(r#"<si(?:\s[^>]*)?>([\s\S]*?)</si>"#)?;
    let texts = regex(r#"<t(?:\s[^>]*)?>([\s\S]*?)</t>"#)?;
    let mut result = Vec::new();
    for string in strings.try_find_iter(xml) {
        let string = string.map_err(|e| e.to_string())?;
        let mut value = String::new();
        for text in texts.try_find_iter(string.get(1).unwrap_or("")) {
            let text = text.map_err(|e| e.to_string())?;
            value.push_str(&decode(text.get(1).unwrap_or("")));
        }
        result.push(value);
    }
    Ok(result)
}

fn cell_value(attributes: &str, body: &str, strings: &[String]) -> Result<SourceValue, String> {
    let kind = first_group(attributes, r#"\bt="([^"]+)""#, 1)?;
    if kind.as_deref() == Some("inlineStr") {
        return Ok(SourceValue::Text(decode(
            &first_group(body, r#"<t(?:\s[^>]*)?>([\s\S]*?)</t>"#, 1)?.unwrap_or_default(),
        )));
    }
    let Some(raw) = first_group(body, r#"<v(?:\s[^>]*)?>([\s\S]*?)</v>"#, 1)? else {
        return Ok(SourceValue::Missing);
    };
    match kind.as_deref() {
        Some("s") => {
            let n = jscompat::to_number(&raw);
            let value = if n.is_finite() && n >= 0.0 && n.fract() == 0.0 {
                strings.get(n as usize)
            } else {
                None
            };
            Ok(value
                .cloned()
                .map(SourceValue::Text)
                .unwrap_or(SourceValue::Missing))
        }
        Some("str") => Ok(SourceValue::Text(decode(&raw))),
        Some("b") => Ok(SourceValue::Number(if raw == "1" { 1.0 } else { 0.0 })),
        _ => {
            let value = jscompat::to_number(&raw);
            if value.is_finite() {
                Ok(SourceValue::Number(value))
            } else {
                Ok(SourceValue::Text(decode(&raw)))
            }
        }
    }
}

fn column_label(mut column: usize) -> String {
    let mut out = Vec::new();
    loop {
        out.push((b'A' + (column % 26) as u8) as char);
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    out.iter().rev().collect()
}

pub(super) fn source_rows(
    worksheet: &str,
    strings: &[String],
    start_row: usize,
    start_column: usize,
    end_row: usize,
    end_column: usize,
) -> Result<Vec<Vec<SourceValue>>, String> {
    let cell = regex(r#"<c\b([^>]*?)\br="([A-Z]+\d+)"([^>]*?)(?:/>|>([\s\S]*?)</c>)"#)?;
    let mut cells = HashMap::new();
    for found in cell.try_find_iter(worksheet) {
        let found = found.map_err(|e| e.to_string())?;
        let Some(address) = found.get(2) else {
            continue;
        };
        if cells.contains_key(address) {
            continue;
        }
        let attrs = format!(
            "{}{}",
            found.get(1).unwrap_or(""),
            found.get(3).unwrap_or("")
        );
        cells.insert(
            address.to_owned(),
            cell_value(&attrs, found.get(4).unwrap_or(""), strings)?,
        );
    }
    let mut rows = Vec::new();
    for row in start_row.saturating_add(1)..=end_row {
        let mut values = Vec::new();
        for column in start_column..=end_column {
            let address = format!("{}{}", column_label(column), row + 1);
            values.push(cells.get(&address).cloned().unwrap_or(SourceValue::Missing));
        }
        rows.push(values);
    }
    Ok(rows)
}

fn date_parts(value: &SourceValue) -> Result<Option<(i32, u32)>, String> {
    use chrono::{Datelike, TimeZone};
    match value {
        SourceValue::Number(value)
            if value.is_finite() && *value >= 0.0 && *value <= 2_958_465.0 =>
        {
            let serial = value.floor();
            if serial == 60.0 {
                return Ok(Some((1900, 2)));
            }
            let days = serial + if serial < 60.0 { 1.0 } else { 0.0 };
            Ok(
                jscompat::utc_date_parts(-2_209_161_600_000.0 + days * 86_400_000.0)
                    .map(|(year, month, _)| (year as i32, month as u32 + 1)),
            )
        }
        SourceValue::Text(value) => {
            let direct = regex(r#"^\s*(\d{4})[-/](\d{1,2})(?:[-/](\d{1,2}))?"#)?;
            if let Some(found) = direct
                .try_find_iter(value)
                .next()
                .transpose()
                .map_err(|e| e.to_string())?
            {
                let year = found
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                let month = found
                    .get(2)
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                return Ok((1..=12).contains(&month).then_some((year, month)));
            }
            let ms = jscompat::date_parse(value);
            if !ms.is_finite() {
                return Ok(None);
            }
            let zoned = TryJsRegex::new(r#"(?:z|(?:gmt|utc)(?:[+-]\d{1,4})?|[+-]\d\d(?::?\d\d)?|est|edt|cst|cdt|mst|mdt|pst|pdt)\s*$"#, "i")
                .map_err(|e| e.to_string())?.try_test(value).map_err(|e| e.to_string())?;
            if zoned {
                Ok(chrono::Local
                    .timestamp_millis_opt(ms as i64)
                    .single()
                    .map(|date| (date.year(), date.month())))
            } else {
                Ok(jscompat::utc_date_parts(ms)
                    .map(|(year, month, _)| (year as i32, month as u32 + 1)))
            }
        }
        _ => Ok(None),
    }
}

fn value_string(value: &SourceValue) -> String {
    match value {
        SourceValue::Missing => String::new(),
        SourceValue::Number(n) => jscompat::number_to_string(*n),
        SourceValue::Text(value) => value.clone(),
    }
}

fn format_boundary(number: f64) -> String {
    if !number.is_finite() || number.abs() >= 1e21 {
        return jscompat::number_to_string(number);
    }
    let bits = number.abs().to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (mantissa, exponent) = if exponent_bits == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1_u64 << 52), exponent_bits - 1023 - 52)
    };
    let scaled = u128::from(mantissa) * 9_765_625_u128;
    let shift = exponent + 10;
    let rounded = if shift >= 0 {
        scaled << (shift as u32)
    } else {
        let right = (-shift) as u32;
        if right >= 128 {
            0
        } else {
            let quotient = scaled >> right;
            let remainder = scaled & ((1_u128 << right) - 1);
            quotient + u128::from(remainder >= (1_u128 << (right - 1)))
        }
    };
    let sign = if number < 0.0 { "-" } else { "" };
    let decimal = format!(
        "{sign}{}.{:010}",
        rounded / 10_000_000_000_u128,
        rounded % 10_000_000_000_u128
    );
    jscompat::number_to_string(jscompat::to_number(&decimal))
}

fn grouped_value(grouping: &serde_json::Value, value: &SourceValue) -> Result<String, String> {
    let raw = value_string(value);
    if raw.is_empty() {
        return Ok(raw);
    }
    if grouping["kind"].as_str() == Some("date") {
        let Some((year, month)) = date_parts(value)? else {
            return Ok(raw);
        };
        return Ok(match grouping["dateUnit"].as_str() {
            Some("year") => year.to_string(),
            Some("quarter") => format!("Q{}", (month - 1) / 3 + 1),
            _ => [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ][(month - 1) as usize]
                .to_owned(),
        });
    }
    let number = match value {
        SourceValue::Number(n) => *n,
        _ => jscompat::to_number(raw.trim()),
    };
    if !number.is_finite() {
        return Ok(raw);
    }
    let start = grouping["rangeStart"].as_f64().unwrap_or(0.0);
    let step = grouping["rangeStep"]
        .as_f64()
        .ok_or("Invalid pivot range step.")?;
    let quotient = (number - start) / step;
    let magnitude = (number.abs() + start.abs()) / step;
    let nearest = jscompat::math_round(quotient);
    let bucket = if (quotient - nearest).abs() <= magnitude * 4.0 * f64::EPSILON {
        nearest
    } else {
        quotient.floor()
    };
    let lower = start + bucket * step;
    Ok(format!(
        "{}-{}",
        format_boundary(lower),
        format_boundary(lower + step)
    ))
}

pub(super) fn build_records(
    rows: &[Vec<SourceValue>],
    model: &super::model::Model<'_>,
) -> Result<String, String> {
    let (row_fields, row_items, col_fields, col_items, page_fields, page_items) =
        model.dimensions();
    let mut dimension_fields: std::collections::HashSet<usize> =
        row_fields.iter().chain(col_fields).copied().collect();
    dimension_fields.extend(page_fields.iter().take(page_items.len()).copied());
    let mut dimensions: HashMap<usize, HashMap<String, usize>> = HashMap::new();
    for (fields, levels) in [
        (row_fields, row_items),
        (col_fields, col_items),
        (page_fields, page_items),
    ] {
        for (field, items) in fields.iter().zip(levels) {
            let mut index = HashMap::new();
            for (position, item) in items.iter().enumerate() {
                index.insert(item.clone(), position);
            }
            dimensions.insert(*field, index);
        }
    }
    let mut groups = HashMap::new();
    for group in model.groupings() {
        let field = group["fieldIndex"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("Invalid pivot grouping field.")?;
        groups.insert(field, group);
    }
    let mut records = String::new();
    for row in rows {
        records.push_str("<r>");
        for (field, value) in row.iter().enumerate() {
            if dimension_fields.contains(&field) {
                let items = dimensions
                    .get(&field)
                    .ok_or("Cannot read properties of undefined (reading 'get')")?;
                let key = if let Some(group) = groups.get(&field) {
                    grouped_value(group, value)?
                } else {
                    value_string(value)
                };
                if let Some(index) = items.get(&key) {
                    records.push_str(&format!("<x v=\"{index}\"/>"));
                } else {
                    records.push_str("<m/>");
                }
            } else {
                match value {
                    SourceValue::Missing => records.push_str("<m/>"),
                    SourceValue::Text(value) if value.is_empty() => records.push_str("<m/>"),
                    SourceValue::Number(value) => records.push_str(&format!(
                        "<n v=\"{}\"/>",
                        jscompat::number_to_string(*value)
                    )),
                    SourceValue::Text(value) => {
                        let n = jscompat::to_number(value);
                        if n.is_finite() {
                            records
                                .push_str(&format!("<n v=\"{}\"/>", jscompat::number_to_string(n)));
                        } else {
                            records.push_str(&format!(
                                "<s v=\"{}\"/>",
                                super::escape_attribute(value)
                            ));
                        }
                    }
                }
            }
        }
        records.push_str("</r>");
    }
    let prefix = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<pivotCacheRecords xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" count=\"{}\"",
        rows.len()
    );
    if rows.is_empty() {
        Ok(format!("{prefix}/>"))
    } else {
        Ok(format!("{prefix}>{records}</pivotCacheRecords>"))
    }
}

#[cfg(test)]
mod tests {
    use super::{SourceValue, shared_strings, source_rows};
    #[test]
    fn reads_shared_inline_numeric_and_blank_cells() {
        let strings =
            shared_strings("<sst><si><r><t>New </t></r><r><t>York</t></r></si></sst>").unwrap();
        let sheet = r#"<worksheet><sheetData><row r="2"><c r="A2" t="s"><v>0</v></c><c r="B2" t="inlineStr"><is><t>A&amp;B</t></is></c><c r="C2"><v>2.5</v></c><c r="D2"/></row></sheetData></worksheet>"#;
        assert_eq!(
            source_rows(sheet, &strings, 0, 0, 1, 3).unwrap(),
            vec![vec![
                SourceValue::Text("New York".into()),
                SourceValue::Text("A&B".into()),
                SourceValue::Number(2.5),
                SourceValue::Missing
            ]]
        );
    }

    #[test]
    fn groups_numeric_boundaries_and_excel_phantom_date() {
        let range = serde_json::json!({"kind":"range","rangeStep":0.1});
        assert_eq!(
            super::grouped_value(&range, &SourceValue::Number(0.3)).unwrap(),
            "0.3-0.4"
        );
        let date = serde_json::json!({"kind":"date","dateUnit":"month"});
        assert_eq!(
            super::grouped_value(&date, &SourceValue::Number(60.0)).unwrap(),
            "Feb"
        );
        assert_eq!(
            super::grouped_value(&date, &SourceValue::Text("not-a-date".into())).unwrap(),
            "not-a-date"
        );
    }
}
